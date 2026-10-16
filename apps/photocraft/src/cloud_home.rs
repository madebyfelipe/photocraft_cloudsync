//! The cloud project library ("Home"): the data and decisions behind the full-window project grid.
//!
//! Everything in this file is plain data and functions with no egui drawing (that is
//! `cloud_home_ui`): card lists built from the sync catalog, search / sort / filter, sync badges,
//! recents persistence, previews (generated on the worker when it serializes a checkpoint, decoded
//! on the worker for the grid), rename validation, the pinned set, the close-without-prompt decision
//! and the worker-side bookkeeping for opens and renames that wait for an on-demand payload.
use crate::cloud_sync::atomic_write;
use craft_cloud_sync::{DownloadedRevision, SyncService};
use photocraft_doc::Document;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// Projects listed in the Recents row.
pub const RECENTS_MAX: usize = 8;
/// A recents file larger than this is treated as corrupt.
const RECENTS_FILE_MAX: u64 = 16 * 1024;
/// Longest project name, as the shared sync client accepts it (it counts bytes).
pub const TITLE_MAX: usize = 256;
/// Longest side of a generated preview, in pixels.
pub const PREVIEW_SIDE: u32 = 256;
/// Largest preview the sync client accepts.
pub const PREVIEW_MAX_BYTES: usize = 512 * 1024;
/// A preview that takes longer than this is skipped, so autosave is never held up by it.
const PREVIEW_BUDGET: Duration = Duration::from_millis(2500);
/// Previews decoded for the grid per request.
pub const PREVIEW_BATCH: usize = 16;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const MIB: u64 = 1024 * 1024;
/// Choices of the "Local cache limit" setting.
pub const CACHE_PRESETS: [u64; 5] = [512 * MIB, 1024 * MIB, 2048 * MIB, 5 * 1024 * MIB, 10 * 1024 * MIB];
/// The shared client's default, and the setting's default.
pub const DEFAULT_CACHE_LIMIT: u64 = 2048 * MIB;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok()).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Cards, badges, search, sort and filter
// ---------------------------------------------------------------------------------------------

/// The sync badge on a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// Local work the server has not received yet.
    NotUploaded,
    /// The payload was requested and has not arrived.
    Downloading,
    /// A conflict copy: `copy` of `of` revisions that all head the same project.
    Conflict { copy: usize, of: usize },
    /// The payload is cached.
    OnThisComputer,
    /// Only the manifest is known; opening downloads the payload.
    InTheCloud,
}

impl Badge {
    pub fn label(self) -> String {
        match self {
            Badge::NotUploaded => "Not uploaded yet".into(),
            Badge::Downloading => "Downloading…".into(),
            Badge::Conflict { copy, of } => format!("Conflict copy {copy}/{of}"),
            Badge::OnThisComputer => "On this computer".into(),
            Badge::InTheCloud => "In the cloud".into(),
        }
    }
}

/// The badge for a revision. Unsent work wins over everything (it is the one state that needs the
/// person's attention), then an outstanding download, then a conflict, then where the payload is.
pub fn badge_for(pending: bool, cached: bool, requested: bool, conflict: Option<(usize, usize)>) -> Badge {
    if pending {
        Badge::NotUploaded
    } else if requested && !cached {
        Badge::Downloading
    } else if let Some((copy, of)) = conflict {
        Badge::Conflict { copy, of }
    } else if cached {
        Badge::OnThisComputer
    } else {
        Badge::InTheCloud
    }
}

/// One project head shown as a card.
#[derive(Clone, Debug)]
pub struct Card {
    pub project: Uuid,
    pub version: Uuid,
    pub title: String,
    pub created_ms: Option<u64>,
    pub cached: bool,
    pub pending: bool,
    /// `(copy, of)` when the project has several heads.
    pub conflict: Option<(usize, usize)>,
    pub preview: Option<PathBuf>,
    pub badge: Badge,
    /// A document of this project is open in the editor.
    pub open: bool,
}

/// One card per head revision. `requested` holds the versions whose payload is being downloaded for
/// an open or a rename; `open_titles` the live name of each open project's document, which is what
/// the next autosave will carry (so a rename shows at once).
pub fn cards_from_catalog(catalog: &[DownloadedRevision], requested: &HashSet<Uuid>, open_titles: &HashMap<Uuid, String>) -> Vec<Card> {
    let mut by_project: HashMap<Uuid, Vec<&DownloadedRevision>> = HashMap::new();
    for r in catalog.iter().filter(|r| r.heads.contains(&r.version_id)) {
        by_project.entry(r.project_id).or_default().push(r);
    }
    for heads in by_project.values_mut() {
        heads.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then_with(|| a.version_id.cmp(&b.version_id)));
        heads.dedup_by_key(|r| r.version_id);
    }
    let mut cards = Vec::new();
    for heads in by_project.values() {
        let of = heads.len();
        for (i, r) in heads.iter().enumerate() {
            let conflict = (of > 1).then_some((i + 1, of));
            let open = open_titles.contains_key(&r.project_id);
            cards.push(Card {
                project: r.project_id,
                version: r.version_id,
                title: open_titles.get(&r.project_id).filter(|_| of == 1).cloned().unwrap_or_else(|| r.title.clone()),
                created_ms: r.created_ms,
                cached: r.cached,
                pending: r.pending,
                conflict,
                preview: r.preview_path.clone(),
                badge: badge_for(r.pending, r.cached, requested.contains(&r.version_id), conflict),
                open,
            });
        }
    }
    sort_cards(&mut cards, Sort::LastEdited);
    cards
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    LastEdited,
    NameAsc,
    NameDesc,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::LastEdited, Sort::NameAsc, Sort::NameDesc];
    pub fn label(self) -> &'static str {
        match self {
            Sort::LastEdited => "Last edited",
            Sort::NameAsc => "Name A–Z",
            Sort::NameDesc => "Name Z–A",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Recents,
    Conflicts,
    /// The payload is cached.
    Offline,
    NotUploaded,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::All, Filter::Recents, Filter::Conflicts, Filter::Offline, Filter::NotUploaded];
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Recents => "Recents",
            Filter::Conflicts => "Conflicts",
            Filter::Offline => "Available offline",
            Filter::NotUploaded => "Not uploaded yet",
        }
    }
}

fn compare(sort: Sort, a: &Card, b: &Card) -> std::cmp::Ordering {
    let (name_a, name_b) = (a.title.to_lowercase(), b.title.to_lowercase());
    match sort {
        // Newest first; a revision without a time last, then by name so the order is stable.
        Sort::LastEdited => b.created_ms.cmp(&a.created_ms).then_with(|| name_a.cmp(&name_b)).then_with(|| a.version.cmp(&b.version)),
        Sort::NameAsc => name_a.cmp(&name_b).then_with(|| a.title.cmp(&b.title)).then_with(|| a.version.cmp(&b.version)),
        Sort::NameDesc => name_b.cmp(&name_a).then_with(|| b.title.cmp(&a.title)).then_with(|| a.version.cmp(&b.version)),
    }
}

fn sort_cards(cards: &mut [Card], sort: Sort) {
    cards.sort_by(|a, b| compare(sort, a, b));
}

/// Indices into `cards` that pass the search text (case-insensitive substring of the title) and the
/// filter, in the chosen order.
pub fn visible(cards: &[Card], query: &str, filter: Filter, sort: Sort, recents: &[Uuid]) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    let mut shown: Vec<usize> = (0..cards.len())
        .filter(|&i| {
            let Some(c) = cards.get(i) else { return false };
            let passes = match filter {
                Filter::All => true,
                Filter::Recents => recents.contains(&c.project),
                Filter::Conflicts => c.conflict.is_some(),
                Filter::Offline => c.cached,
                Filter::NotUploaded => c.pending,
            };
            passes && (needle.is_empty() || c.title.to_lowercase().contains(&needle))
        })
        .collect();
    shown.sort_by(|&a, &b| match (cards.get(a), cards.get(b)) {
        (Some(x), Some(y)) => compare(sort, x, y),
        _ => std::cmp::Ordering::Equal,
    });
    shown
}

/// The Recents row: the newest card of each recently opened project, most recent first.
pub fn recent_cards(cards: &[Card], recents: &[Uuid]) -> Vec<usize> {
    let mut out = Vec::new();
    for project in recents {
        let best = (0..cards.len()).filter(|&i| cards.get(i).is_some_and(|c| c.project == *project)).max_by_key(|&i| cards.get(i).and_then(|c| c.created_ms));
        if let Some(i) = best {
            out.push(i);
        }
        if out.len() == RECENTS_MAX {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------------------------

/// "Edited 5 minutes ago"; "Edited —" when the revision has no time.
pub fn edited_label(created_ms: Option<u64>, now_ms: u64) -> String {
    let Some(created) = created_ms else { return "Edited —".into() };
    let secs = now_ms.saturating_sub(created) / 1000;
    let plural = |n: u64, unit: &str| format!("Edited {n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        0..=59 => "Edited just now".into(),
        60..=3599 => plural(secs / 60, "minute"),
        3600..=86_399 => plural(secs / 3600, "hour"),
        86_400..=172_799 => "Edited yesterday".into(),
        172_800..=2_591_999 => plural(secs / 86_400, "day"),
        _ => {
            const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            let (y, m, d) = civil_from_days(i64::try_from(created / 86_400_000).unwrap_or(0));
            format!("Edited {} {d}, {y}", MONTHS.get(usize::try_from(m.saturating_sub(1)).unwrap_or(0)).copied().unwrap_or("—"))
        }
    }
}

/// Days since 1970-01-01 to a (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// "Cache: 120.5 MB of 2.0 GB".
pub fn cache_label(used: u64, limit: u64) -> String {
    format!("Cache: {} of {}", bytes_label(used), bytes_label(limit))
}

pub fn bytes_label(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * MIB {
        format!("{:.1} GB", b / (1024.0 * MIB as f64))
    } else if bytes >= MIB {
        format!("{:.1} MB", b / MIB as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// A project name the person typed: trimmed, 1–256 characters, no control characters.
pub fn validate_title(raw: &str) -> Result<String, &'static str> {
    let title = raw.trim();
    if title.is_empty() {
        Err("The name can't be empty.")
    } else if title.chars().any(char::is_control) {
        Err("The name can't contain control characters.")
    } else if title.chars().count() > TITLE_MAX || title.len() > TITLE_MAX {
        Err("The name is too long (256 characters at most, fewer for non-Latin text).")
    } else {
        Ok(title.to_owned())
    }
}

// ---------------------------------------------------------------------------------------------
// Recents and settings on disk
// ---------------------------------------------------------------------------------------------

/// The projects this app opened most recently on this computer, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recents {
    entries: Vec<(Uuid, u64)>,
}

impl Recents {
    /// Reads `{"version":1,"projects":[{"project":"<uuid>","opened_ms":n},…]}`. Anything else
    /// (corrupt, truncated, not an object) is an empty list; bad entries are skipped.
    pub fn parse(bytes: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else { return Self::default() };
        let mut entries: Vec<(Uuid, u64)> = Vec::new();
        for entry in value.get("projects").and_then(serde_json::Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            let project = entry.get("project").and_then(serde_json::Value::as_str).and_then(|s| Uuid::parse_str(s).ok());
            let opened = entry.get("opened_ms").and_then(serde_json::Value::as_u64).unwrap_or(0);
            if let Some(project) = project
                && !entries.iter().any(|(p, _)| *p == project)
            {
                entries.push((project, opened));
            }
        }
        entries.sort_by_key(|(_, opened)| std::cmp::Reverse(*opened));
        entries.truncate(RECENTS_MAX);
        Self { entries }
    }

    /// A missing, unreadable, oversized or corrupt file is an empty list.
    pub fn load(path: &Path) -> Self {
        let Ok(file) = std::fs::File::open(path) else { return Self::default() };
        let mut bytes = Vec::new();
        if file.take(RECENTS_FILE_MAX + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > RECENTS_FILE_MAX {
            return Self::default();
        }
        Self::parse(&bytes)
    }

    /// Opening a project moves it to the front.
    pub fn touch(&mut self, project: Uuid, now_ms: u64) {
        self.entries.retain(|(p, _)| *p != project);
        self.entries.insert(0, (project, now_ms));
        self.entries.truncate(RECENTS_MAX);
    }

    pub fn ids(&self) -> Vec<Uuid> {
        self.entries.iter().map(|(p, _)| *p).collect()
    }

    pub fn entries(&self) -> &[(Uuid, u64)] {
        &self.entries
    }

    pub fn from_entries(entries: Vec<(Uuid, u64)>) -> Self {
        let mut me = Self { entries };
        me.entries.truncate(RECENTS_MAX);
        me
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let projects: Vec<_> = self.entries.iter().map(|(p, t)| serde_json::json!({"project": p.to_string(), "opened_ms": t})).collect();
        serde_json::to_vec(&serde_json::json!({"version": 1, "projects": projects})).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        atomic_write(path, &self.to_bytes())
    }
}

pub fn recents_path(dir: &Path) -> PathBuf {
    dir.join("recents.json")
}

pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join("home.json")
}

/// The saved cache limit; anything that is not one of the presets is the default.
pub fn load_cache_limit(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else { return DEFAULT_CACHE_LIMIT };
    let mut bytes = Vec::new();
    if file.take(4096).read_to_end(&mut bytes).is_err() {
        return DEFAULT_CACHE_LIMIT;
    }
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("cache_limit").and_then(serde_json::Value::as_u64))
        .filter(|limit| CACHE_PRESETS.contains(limit))
        .unwrap_or(DEFAULT_CACHE_LIMIT)
}

pub fn save_cache_limit(path: &Path, limit: u64) -> Result<(), String> {
    atomic_write(path, &serde_json::to_vec(&serde_json::json!({"cache_limit": limit})).map_err(|e| e.to_string())?)
}

// ---------------------------------------------------------------------------------------------
// Open documents: pinning and the close decision
// ---------------------------------------------------------------------------------------------

/// What the adapter knows about an open document's cloud identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudDoc {
    /// The project this document autosaves into, once known.
    pub project: Option<Uuid>,
    /// The newest revision this document is bound to.
    pub version: Option<Uuid>,
    /// Created from Home or opened from the cloud: it has no file of its own, the cloud copy is the document.
    pub cloud: bool,
}

/// The revisions open in the editor: their payloads must never be evicted from the cache.
pub fn pinned_set<'a>(docs: impl IntoIterator<Item = &'a CloudDoc>) -> BTreeSet<Uuid> {
    docs.into_iter().filter_map(|d| d.version).collect()
}

/// What closing or quitting does with an unsaved document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseDecision {
    /// The current revision is already in the local checkpoint: nothing to ask.
    Safe,
    /// A cloud document that is not durable yet: write the checkpoint first.
    Flush,
    /// Keep the editor's normal unsaved-changes prompt (a file on disk, or not a cloud document).
    Prompt,
}

pub fn close_decision(has_path: bool, cloud: bool, durable: Option<u64>, revision: u64) -> CloseDecision {
    if has_path || !cloud {
        CloseDecision::Prompt
    } else if durable == Some(revision) {
        CloseDecision::Safe
    } else {
        CloseDecision::Flush
    }
}

// ---------------------------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------------------------

fn encode_thumbnail(doc: &Document, side: u32) -> Option<Vec<u8>> {
    if doc.bounds().is_empty() {
        return None;
    }
    let thumb = photocraft_compose::thumbnail(doc, side);
    let (w, h) = (thumb.width, thumb.height);
    if w == 0 || h == 0 || w > side || h > side || thumb.pixels.len() != (w as usize).checked_mul(h as usize)?.checked_mul(4)? {
        return None;
    }
    let image = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgba, thumb.pixels).ok()?;
    photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &photocraft_codecs::EncodeOptions::default()).ok()
}

/// A flattened PNG of the document, longest side at most 256 px and at most 512 KiB (the size drops
/// if a PNG is too large). `None` for anything that cannot be rendered or encoded.
pub fn preview_png(doc: &Document) -> Option<Vec<u8>> {
    let mut side = PREVIEW_SIDE;
    loop {
        let png = encode_thumbnail(doc, side)?;
        if png.len() <= PREVIEW_MAX_BYTES && png.starts_with(PNG_SIGNATURE) {
            return Some(png);
        }
        if side <= 32 {
            return None;
        }
        side /= 2;
    }
}

/// [`preview_png`] on a helper thread that is given `budget`; a render that overruns is abandoned
/// (it finishes in the background and its result is dropped) and the checkpoint goes without a
/// preview. Only one such render runs at a time: while `busy` is set the answer is `None` at once.
pub fn preview_png_bounded(doc: Arc<Document>, budget: Duration, busy: &Arc<AtomicBool>) -> Option<Vec<u8>> {
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    if busy.swap(true, Ordering::AcqRel) {
        return None;
    }
    let release = Release(Arc::clone(busy));
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("photocraft-preview".into()).spawn(move || {
        let _release = release;
        let _ = tx.send(preview_png(&doc));
    });
    spawned.ok()?;
    rx.recv_timeout(budget).ok().flatten()
}

/// A preview ready to become a texture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

/// Width and height from a PNG's IHDR chunk, without decoding anything.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(PNG_SIGNATURE) || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let be = |at: usize| bytes.get(at..at + 4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_be_bytes);
    Some((be(16)?, be(20)?))
}

/// Reads and decodes a preview the sync client cached. Anything wrong with it (missing, too large,
/// not a PNG, absurd dimensions, undecodable) is `None`: the card shows a placeholder.
pub fn decode_preview(path: &Path) -> Option<Decoded> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(PREVIEW_MAX_BYTES as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > PREVIEW_MAX_BYTES {
        return None;
    }
    let (w, h) = png_dimensions(&bytes)?;
    if w == 0 || h == 0 || w > 1024 || h > 1024 {
        return None;
    }
    let image = photocraft_codecs::decode(&bytes).ok()?;
    let image = image.convert(photocraft_codecs::ChannelLayout::Rgba, photocraft_codecs::SampleType::U8);
    let rgba = image.as_u8()?;
    let (w, h) = (image.width() as usize, image.height() as usize);
    (rgba.len() == w.checked_mul(h)?.checked_mul(4)?).then(|| Decoded { size: [w, h], rgba: rgba.to_vec() })
}

// ---------------------------------------------------------------------------------------------
// Cleaning up the adapter's working copies
// ---------------------------------------------------------------------------------------------

/// Files of `key` in the working directory are `<key>-<64 hex digits>.pcraft`.
fn is_payload_of(key: &str, name: &str) -> bool {
    name.strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".pcraft"))
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Removes the checkpoints of `key` that a newer one replaced; `keep` is the newest payload file name.
/// Only called once the newer checkpoint's metadata is on disk, so the newest state is never at risk.
pub fn prune_superseded(working: &Path, key: &str, keep: &str) {
    let Ok(entries) = std::fs::read_dir(working) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name != keep && is_payload_of(key, name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A checkpoint whose `local_token` the sync queue has just reported as `Published` may leave the
/// working directory when all of this holds: its document is not open (`open_keys`), it is the
/// newest checkpoint of its key (the metadata is replaced by every checkpoint, so a newer one would
/// carry another token), it was handed to the queue (`needs_submit` is false) and it belongs to a
/// document without a file of its own (a file's key must keep mapping to its cloud project).
/// Returns the key that was cleaned up.
pub fn cleanup_published(working: &Path, token: &str, open_keys: &HashSet<String>) -> Option<String> {
    for entry in std::fs::read_dir(working).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        if meta.get("local_token").and_then(serde_json::Value::as_str) != Some(token) {
            continue;
        }
        let key = meta.get("key").and_then(serde_json::Value::as_str)?;
        let payload = meta.get("payload").and_then(serde_json::Value::as_str)?;
        let eligible = !open_keys.contains(key)
            && meta.get("needs_submit").and_then(serde_json::Value::as_bool) == Some(false)
            && meta.get("portable").and_then(serde_json::Value::as_bool) == Some(true)
            && meta.get("source").is_none_or(serde_json::Value::is_null)
            && is_payload_of(key, payload)
            && path.file_name().and_then(|n| n.to_str()) == Some(&format!("{key}.json"));
        if !eligible {
            return None;
        }
        // The metadata first: a crash in between leaves an unreferenced file, never a listed copy without its payload.
        std::fs::remove_file(&path).ok()?;
        let _ = std::fs::remove_file(working.join(payload));
        prune_superseded(working, key, payload);
        return Some(key.to_owned());
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Worker side
// ---------------------------------------------------------------------------------------------

/// Requests from the Home screen to the worker thread.
pub enum HomeCommand {
    /// Download the revision if needed, then open it bound to `project`.
    OpenVersion {
        project: Uuid,
        version: Uuid,
        key: String,
    },
    /// Rename a project that is not open: download the payload if needed, then publish a renamed revision.
    Rename {
        project: Uuid,
        version: Uuid,
        title: String,
    },
    /// Decode these cached previews for the grid.
    Previews(Vec<(Uuid, PathBuf)>),
    /// The revisions open in the editor.
    Pin(BTreeSet<Uuid>),
    CacheLimit(u64),
    Recents(Vec<(Uuid, u64)>),
    /// The working keys of the documents open in the editor (their checkpoints are never cleaned up).
    OpenKeys(HashSet<String>),
}

pub enum HomeReply {
    /// An open or rename could not finish; shown on the card.
    Failed {
        version: Uuid,
        error: String,
    },
    /// The renamed revision was queued for upload.
    Renamed {
        version: Uuid,
        new_version: Uuid,
    },
    Previews(Vec<(Uuid, Option<Decoded>)>),
}

/// An open waiting for its payload.
pub struct HomeOpen {
    pub key: String,
    pub project: Uuid,
    pub version: Uuid,
}

struct HomeRename {
    project: Uuid,
    version: Uuid,
    title: String,
}

pub struct Startup {
    pub recents: Vec<(Uuid, u64)>,
    pub cache_limit: u64,
}

/// The worker's Home bookkeeping. It never touches the UI: it returns replies for the caller to send.
pub struct HomeWorker {
    opens: Vec<HomeOpen>,
    renames: Vec<HomeRename>,
    cache_limit: u64,
    pinned: BTreeSet<Uuid>,
    open_keys: HashSet<String>,
    dir: PathBuf,
    /// Set while a preview render is running (see [`preview_png_bounded`]).
    pub preview_busy: Arc<AtomicBool>,
}

impl HomeWorker {
    pub fn new(dir: &Path) -> (Self, Startup) {
        let cache_limit = load_cache_limit(&settings_path(dir));
        let recents = Recents::load(&recents_path(dir)).entries().to_vec();
        let me = Self {
            opens: Vec::new(),
            renames: Vec::new(),
            cache_limit,
            pinned: BTreeSet::new(),
            open_keys: HashSet::new(),
            dir: dir.to_path_buf(),
            preview_busy: Arc::new(AtomicBool::new(false)),
        };
        (me, Startup { recents, cache_limit })
    }

    /// Re-applies the cache limit and the pinned set; a new `SyncService` starts with the defaults.
    pub fn apply(&self, service: &SyncService) {
        service.set_cache_limit(self.cache_limit);
        service.set_pinned(self.pinned.clone());
    }

    pub fn preview(&self, doc: &Arc<Document>) -> Option<Vec<u8>> {
        preview_png_bounded(Arc::clone(doc), PREVIEW_BUDGET, &self.preview_busy)
    }

    pub fn handle(&mut self, command: HomeCommand, service: Option<&SyncService>) -> Vec<HomeReply> {
        match command {
            HomeCommand::OpenVersion { project, version, key } => match service {
                Some(service) => {
                    self.opens.push(HomeOpen { key, project, version });
                    service.request_payload(version);
                    Vec::new()
                }
                None => vec![HomeReply::Failed { version, error: "Sync queue unavailable; try again after reconnecting.".into() }],
            },
            HomeCommand::Rename { project, version, title } => match (service, validate_title(&title)) {
                (Some(service), Ok(title)) => {
                    self.renames.push(HomeRename { project, version, title });
                    service.request_payload(version);
                    Vec::new()
                }
                (None, _) => vec![HomeReply::Failed { version, error: "Sync queue unavailable; try again after reconnecting.".into() }],
                (_, Err(why)) => vec![HomeReply::Failed { version, error: why.into() }],
            },
            HomeCommand::Previews(wanted) => {
                vec![HomeReply::Previews(wanted.into_iter().take(PREVIEW_BATCH).map(|(version, path)| (version, decode_preview(&path))).collect())]
            }
            HomeCommand::Pin(versions) => {
                if let Some(service) = service {
                    service.set_pinned(versions.clone());
                }
                self.pinned = versions;
                Vec::new()
            }
            HomeCommand::CacheLimit(limit) => {
                self.cache_limit = limit;
                if let Some(service) = service {
                    service.set_cache_limit(limit);
                }
                match save_cache_limit(&settings_path(&self.dir), limit) {
                    Ok(()) => Vec::new(),
                    Err(e) => vec![HomeReply::Failed { version: Uuid::nil(), error: format!("Cannot save the cache limit: {e}") }],
                }
            }
            HomeCommand::OpenKeys(keys) => {
                self.open_keys = keys;
                Vec::new()
            }
            HomeCommand::Recents(entries) => {
                if let Err(e) = Recents::from_entries(entries).save(&recents_path(&self.dir)) {
                    log::warn!("cannot save recent projects: {e}");
                }
                Vec::new()
            }
        }
    }

    /// The sync queue published the checkpoint with this token: its working copy goes if nothing needs it any more.
    pub fn published(&self, token: &str) -> Option<String> {
        cleanup_published(&self.dir.join("working"), token, &self.open_keys)
    }

    /// A payload arrived (or was already cached): the opens that waited for it, and the renames done now.
    pub fn fetched(&mut self, version: Uuid, service: Option<&SyncService>) -> (Vec<HomeOpen>, Vec<HomeReply>) {
        let (opens, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.opens).into_iter().partition(|o| o.version == version);
        self.opens = rest;
        let (renames, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.renames).into_iter().partition(|r| r.version == version);
        self.renames = rest;
        let replies = renames
            .into_iter()
            .map(|r| match service.map(|s| s.rename(r.project, r.version, &r.title)) {
                Some(Ok(submission)) => HomeReply::Renamed { version, new_version: submission.version_id },
                Some(Err(e)) => HomeReply::Failed { version, error: format!("Cannot rename: {e}") },
                None => HomeReply::Failed { version, error: "Sync queue unavailable; try again after reconnecting.".into() },
            })
            .collect();
        (opens, replies)
    }

    /// A payload could not be fetched (offline counts): the opens and renames that waited for it fail.
    pub fn fetch_failed(&mut self, version: Uuid, error: &str) -> Vec<HomeReply> {
        let opens = self.opens.iter().filter(|o| o.version == version).count();
        let renames = self.renames.iter().filter(|r| r.version == version).count();
        self.opens.retain(|o| o.version != version);
        self.renames.retain(|r| r.version != version);
        (0..opens + renames).map(|_| HomeReply::Failed { version, error: format!("Cannot download this project: {error}") }).collect()
    }

    /// The connection changed: everything still waiting for a download is dropped and reported.
    pub fn interrupt(&mut self) -> Vec<HomeReply> {
        let mut versions: Vec<Uuid> = self.opens.drain(..).map(|o| o.version).collect();
        versions.extend(self.renames.drain(..).map(|r| r.version));
        versions.into_iter().map(|version| HomeReply::Failed { version, error: "Interrupted by reconnecting; try again.".into() }).collect()
    }

    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.opens.len() + self.renames.len()
    }
}

// ---------------------------------------------------------------------------------------------
// Control channel
// ---------------------------------------------------------------------------------------------

/// Splits the control server's requests: `home.*` goes to the Home screen, everything else to the
/// editor, in order. The thread ends when the server or either receiver goes away.
pub fn split_control(
    rx: mpsc::Receiver<photocraft_ui_egui::ControlRequest>,
    ctx: egui::Context,
) -> (mpsc::Receiver<photocraft_ui_egui::ControlRequest>, mpsc::Receiver<photocraft_ui_egui::ControlRequest>) {
    let (editor_tx, editor_rx) = mpsc::channel();
    let (home_tx, home_rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("photocraft-control-router".into()).spawn(move || {
        for request in rx {
            let target = if request.method.starts_with("home.") { &home_tx } else { &editor_tx };
            if target.send(request).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    if let Err(e) = spawned {
        log::warn!("control router unavailable: {e}");
    }
    (editor_rx, home_rx)
}

/// `home.list`: the library as an agent sees it.
pub fn list_json(cards: &[Card], ready: bool, connected: bool, status: &str, now_ms: u64) -> serde_json::Value {
    let projects: Vec<_> = cards
        .iter()
        .map(|c| {
            serde_json::json!({
                "project": c.project.to_string(),
                "version": c.version.to_string(),
                "title": c.title,
                "created_ms": c.created_ms,
                "edited": edited_label(c.created_ms, now_ms),
                "badge": c.badge.label(),
                "cached": c.cached,
                "pending": c.pending,
                "conflict": c.conflict.map(|(copy, of)| serde_json::json!({"copy": copy, "of": of})),
                "hasPreview": c.preview.is_some(),
                "open": c.open,
            })
        })
        .collect();
    serde_json::json!({"ready": ready, "connected": connected, "status": status, "projects": projects})
}

#[cfg(test)]
mod tests;
