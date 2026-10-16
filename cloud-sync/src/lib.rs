#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
//! Durable native-file snapshots over a dedicated WebDAV root. Local submission is not a remote save acknowledgement.
pub mod credentials;
use reqwest::{
    blocking::{Client, Response},
    Method, StatusCode, Url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub struct Error(pub String);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self(e.to_string())
    }
}
const CONNECTION_UNAVAILABLE: &str = "WebDAV connection unavailable";
const CANCELLED: &str = "sync cancelled";
const OFFLINE_FETCH: &str = "offline: the revision payload is not downloaded and the WebDAV connection is unavailable";
const NOT_LOCAL: &str = "revision payload is not available locally; request it first";
const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;
const MAX_PREVIEW: u64 = 512 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_LIST: u64 = 4 * 1024 * 1024;
const DEFAULT_CACHE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
/// A payload used this recently (fetched, requested, submitted, pinned) is about to be opened and is not evicted.
const RECENT_MS: u64 = 60_000;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
#[derive(Clone)]
pub struct Config {
    pub endpoint: String,
    pub username: String,
    pub password: String,
}
pub struct Snapshot {
    pub local_key: String,
    pub title: String,
    pub extension: String,
    pub local_token: String,
    pub native: Vec<u8>,
    /// Optional PNG thumbnail (signature checked, at most 512 KiB), published as `<version>.preview` before the manifest.
    pub preview: Option<Vec<u8>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub project_id: Uuid,
    pub version_id: Uuid,
}
/// One catalog entry: a revision whose manifest is known. The payload is downloaded on demand (`request_payload`).
#[derive(Clone, Debug)]
pub struct DownloadedRevision {
    pub project_id: Uuid,
    pub version_id: Uuid,
    pub parents: Vec<Uuid>,
    pub heads: Vec<Uuid>,
    pub title: String,
    pub extension: String,
    /// Payload location; the file exists and is valid only when `cached`.
    pub local_path: PathBuf,
    /// Wall-clock ms since the UNIX epoch when the revision was submitted (absent for revisions from older clients).
    pub created_ms: Option<u64>,
    /// Payload size in bytes, from the verified manifest.
    pub size: u64,
    /// Verified PNG preview on disk, if the revision has one and it was downloaded.
    pub preview_path: Option<PathBuf>,
    /// Payload present locally with the manifest size (checksummed on startup and on download).
    pub cached: bool,
    /// Not yet published to the remote.
    pub pending: bool,
}
#[derive(Clone, Debug)]
pub enum SyncEvent {
    Durable {
        project_id: Uuid,
        version_id: Uuid,
        local_token: String,
    },
    Published {
        project_id: Uuid,
        version_id: Uuid,
        local_token: String,
    },
    /// A revision first appeared in the catalog (manifest known; check `cached` for the payload).
    Downloaded(DownloadedRevision),
    /// A requested payload is verified and present at `path`.
    Fetched {
        project_id: Uuid,
        version_id: Uuid,
        path: PathBuf,
    },
    /// A requested payload could not be fetched (offline counts); the request is dropped and may be repeated.
    FetchFailed {
        version_id: Uuid,
        error: String,
    },
    Error(String),
}
#[derive(Clone, Debug, Default)]
pub enum SyncStatus {
    #[default]
    Idle,
    Syncing,
    Offline,
    Error(String),
}
#[derive(Clone, Debug)]
pub struct SyncState {
    pub status: SyncStatus,
    pub pending: usize,
    pub catalog: Vec<DownloadedRevision>,
    pub events: Vec<SyncEvent>,
    /// Bytes currently used by cached payloads.
    pub cache_bytes: u64,
    /// Payload cache limit in bytes (default 2 GiB); see `SyncService::set_cache_limit`.
    pub cache_limit: u64,
}
impl Default for SyncState {
    fn default() -> Self {
        Self { status: SyncStatus::default(), pending: 0, catalog: Vec::new(), events: Vec::new(), cache_bytes: 0, cache_limit: DEFAULT_CACHE_LIMIT }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PreviewRef {
    size: u64,
    sha256: String,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Manifest {
    schema: u32,
    app: String,
    project: Uuid,
    version: Uuid,
    parents: Vec<Uuid>,
    title: String,
    extension: String,
    size: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview: Option<PreviewRef>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    manifest: Manifest,
    local_token: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    project: Uuid,
    version: Uuid,
    sha256: String,
    title: String,
    extension: String,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Journal {
    #[serde(default)]
    receipts: BTreeMap<String, BTreeMap<String, Receipt>>,
    bindings: BTreeMap<String, (Uuid, Uuid)>,
    pending: Vec<Pending>,
    /// Last access (ms since the UNIX epoch) per revision; drives least-recently-used payload eviction.
    #[serde(default)]
    access: BTreeMap<Uuid, u64>,
}
#[derive(Default)]
struct Shared {
    journal: Journal,
    state: SyncState,
    /// Payload downloads requested by the application, served by the worker.
    requests: BTreeSet<Uuid>,
    /// Revisions open in the editor; their payloads are never evicted.
    pinned: BTreeSet<Uuid>,
    /// Revisions seen in the latest remote listing or published by this client; only these may be evicted.
    confirmed: BTreeSet<Uuid>,
    /// Previews that failed verification this session (reported once, not retried).
    bad_previews: BTreeSet<Uuid>,
    /// `journal.access` changed in memory and is not yet persisted.
    dirty: bool,
}
pub struct SyncService {
    lease: Arc<File>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    wake: std::sync::mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
    cache: PathBuf,
    app: String,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error("cache path has no parent".into()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| Error(e.error.to_string()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn journal(cache: &Path, j: &Journal) -> Result<()> {
    {
        let bytes = serde_json::to_vec(j)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(Error("local outbox capacity reached".into()));
        }
        atomic(&cache.join("journal.json"), &bytes)
    }
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn now_ms() -> Option<u64> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok())
}
fn valid_label(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
impl SyncService {
    pub fn new(config: Config, cache_dir: impl AsRef<Path>, app: &str) -> Result<Self> {
        if !valid_label(app) {
            return Err(Error("invalid app identifier".into()));
        }
        let mut remote = if config.endpoint.is_empty() { None } else { Some(Remote::new(config)?) };
        let cache = cache_dir.as_ref().to_path_buf();
        fs::create_dir_all(cache.join("payloads"))?;
        fs::create_dir_all(cache.join("previews"))?;
        fs::create_dir_all(cache.join("manifests"))?;
        let lease = Arc::new(fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(cache.join("cache.lock"))?);
        fs2::FileExt::try_lock_exclusive(lease.as_ref()).map_err(|_| Error("sync cache is already open by another service".into()))?;
        if let Some(ref r) = remote {
            let scope = hash(format!("{}\n{}\n{}", app, r.base, r.user).as_bytes());
            let file = cache.join("account-scope");
            if file.exists() {
                if bounded_file(&file, 1024)? != scope.as_bytes() {
                    return Err(Error("cache belongs to a different WebDAV account or endpoint; use a separate cache directory".into()));
                }
            } else {
                atomic(&file, scope.as_bytes())?;
            }
        }
        let j: Journal = if cache.join("journal.json").exists() {
            serde_json::from_slice(&bounded_file(&cache.join("journal.json"), 16 * 1024 * 1024)?)?
        } else {
            Journal::default()
        };
        let mut all: BTreeMap<Uuid, Manifest> = BTreeMap::new();
        for entry in fs::read_dir(cache.join("manifests"))? {
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) == Some("json") {
                let m: Manifest = serde_json::from_slice(&bounded_file(&path, MAX_MANIFEST)?)?;
                validate(&m, app)?;
                all.insert(m.version, m);
            }
        }
        let pending: BTreeSet<Uuid> = j.pending.iter().map(|p| p.manifest.version).collect();
        for p in &j.pending {
            validate(&p.manifest, app)?;
            all.insert(p.manifest.version, p.manifest.clone());
        }
        for m in all.values() {
            let path = payload(&cache, m.version);
            let sound = |b: Vec<u8>| b.len() as u64 == m.size && hash(&b) == m.sha256;
            if pending.contains(&m.version) {
                if !sound(bounded_file(&path, MAX_PAYLOAD)?) {
                    return Err(Error("cached revision payload checksum mismatch".into()));
                }
            } else {
                // A published payload may be missing (evicted or never fetched); a damaged one is dropped, not fatal.
                if path.exists() && !bounded_file(&path, MAX_PAYLOAD).is_ok_and(sound) {
                    let _ = fs::remove_file(&path);
                }
                if let Some(pr) = &m.preview {
                    if preview_file(&cache, m.version).exists() && read_preview(&cache, m.version, pr).is_none() {
                        let _ = fs::remove_file(preview_file(&cache, m.version));
                    }
                }
            }
        }
        let initial = build_catalog(&cache, all.values(), &pending);
        let state = SyncState { catalog: initial, cache_bytes: payload_bytes(&cache), ..SyncState::default() };
        let shared = Arc::new(Mutex::new(Shared { journal: j, state, ..Shared::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(ref mut r) = remote {
            r.cancel = Some(stop.clone());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let s = shared.clone();
        let st = stop.clone();
        let c = cache.clone();
        let a = app.to_owned();
        let worker = thread::Builder::new().name("craft-cloud-sync".into()).spawn(move || {
            while !st.load(Ordering::Acquire) {
                if let Some(ref remote) = remote {
                    lock(&s).state.status = SyncStatus::Syncing;
                    if let Err(e) = cycle(remote, &c, &a, &s, &st) {
                        let mut guard = lock(&s);
                        if st.load(Ordering::Acquire) || e.0 == CANCELLED {
                            break;
                        }
                        if e.0 == CONNECTION_UNAVAILABLE {
                            guard.state.status = SyncStatus::Offline;
                        } else {
                            guard.state.status = SyncStatus::Error(e.0.clone());
                            guard.state.events.push(SyncEvent::Error(e.0));
                        }
                    } else {
                        lock(&s).state.status = SyncStatus::Idle;
                    }
                } else {
                    serve(None, &c, &a, &s, &st);
                    lock(&s).state.status = SyncStatus::Offline;
                }
                trim_events(&mut lock(&s).state.events);
                let _ = rx.recv_timeout(Duration::from_secs(15));
            }
        })?;
        Ok(Self { lease, shared, stop, wake: tx, worker: Some(worker), cache, app: app.into() })
    }
    pub fn submitter(&self) -> Submitter {
        Submitter {
            stop: self.stop.clone(),
            lease: self.lease.clone(),
            shared: self.shared.clone(),
            wake: self.wake.clone(),
            cache: self.cache.clone(),
            app: self.app.clone(),
        }
    }
    pub fn submit(&self, s: Snapshot) -> Result<Submission> {
        self.submitter().submit(s)
    }
    pub fn bind(&self, local_key: &str, project_id: Uuid, version_id: Uuid) -> Result<()> {
        self.submitter().bind(local_key, project_id, version_id)
    }
    /// See [`Submitter::rename`].
    pub fn rename(&self, project_id: Uuid, version_id: Uuid, title: &str) -> Result<Submission> {
        self.submitter().rename(project_id, version_id, title)
    }
    /// See [`Submitter::request_payload`].
    pub fn request_payload(&self, version_id: Uuid) {
        self.submitter().request_payload(version_id)
    }
    /// See [`Submitter::set_pinned`].
    pub fn set_pinned(&self, versions: BTreeSet<Uuid>) {
        self.submitter().set_pinned(versions)
    }
    /// See [`Submitter::set_cache_limit`].
    pub fn set_cache_limit(&self, bytes: u64) {
        self.submitter().set_cache_limit(bytes)
    }
    pub fn poll(&self) -> SyncState {
        let mut s = lock(&self.shared);
        SyncState {
            status: s.state.status.clone(),
            pending: s.journal.pending.len(),
            catalog: s.state.catalog.clone(),
            events: std::mem::take(&mut s.state.events),
            cache_bytes: s.state.cache_bytes,
            cache_limit: s.state.cache_limit,
        }
    }
    /// One entry per head revision (a project with conflict copies yields several), newest first; revisions without `created_ms` last.
    pub fn catalog_heads(&self) -> Vec<DownloadedRevision> {
        let mut heads: Vec<_> = lock(&self.shared).state.catalog.iter().filter(|r| r.heads.contains(&r.version_id)).cloned().collect();
        heads.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| a.version_id.cmp(&b.version_id)));
        heads
    }
    pub fn request_sync(&self) {
        let _ = self.wake.send(());
    }
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.send(());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
            let _guard = lock(&self.shared);
            let _ = fs2::FileExt::unlock(self.lease.as_ref());
        }
    }
}
#[derive(Clone)]
pub struct Submitter {
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    lease: Arc<File>,
    shared: Arc<Mutex<Shared>>,
    wake: std::sync::mpsc::Sender<()>,
    cache: PathBuf,
    app: String,
}
impl Submitter {
    pub fn submit(&self, s: Snapshot) -> Result<Submission> {
        if !valid_label(&s.local_key) || !valid_label(&s.title) || s.local_token.len() > 1024 || s.native.len() as u64 > MAX_PAYLOAD || !extension(&s.extension)
        {
            return Err(Error("invalid snapshot metadata or payload size".into()));
        }
        if s.preview.as_ref().is_some_and(|p| p.len() as u64 > MAX_PREVIEW || !p.starts_with(PNG_SIGNATURE)) {
            return Err(Error("invalid preview: expected a PNG of at most 512 KiB".into()));
        }
        let mut state = lock(&self.shared);
        if self.stop.load(Ordering::Acquire) {
            return Err(Error("sync service has stopped".into()));
        }
        let digest = hash(&s.native);
        if let Some(receipt) = state.journal.receipts.get(&s.local_key).and_then(|tokens| tokens.get(&s.local_token)).cloned() {
            if receipt.sha256 != digest || receipt.title != s.title || receipt.extension != s.extension {
                return Err(Error("snapshot token reused for different content; use a new token".into()));
            }
            state.state.events.push(SyncEvent::Durable { project_id: receipt.project, version_id: receipt.version, local_token: s.local_token });
            return Ok(Submission { project_id: receipt.project, version_id: receipt.version });
        }
        let (project, parent) = state.journal.bindings.get(&s.local_key).map(|(p, v)| (*p, Some(*v))).unwrap_or_else(|| (Uuid::new_v4(), None));
        let version = Uuid::new_v4();
        let m = Manifest {
            schema: 1,
            app: self.app.clone(),
            project,
            version,
            parents: parent.into_iter().collect(),
            title: s.title,
            extension: s.extension,
            size: s.native.len() as u64,
            sha256: digest,
            created_ms: now_ms(),
            preview: s.preview.as_deref().map(|p| PreviewRef { size: p.len() as u64, sha256: hash(p) }),
        };
        let receipt = Receipt { project, version, sha256: m.sha256.clone(), title: m.title.clone(), extension: m.extension.clone() };
        let (key, token) = (s.local_key, s.local_token);
        enqueue(&self.cache, &mut state, m, &s.native, s.preview.as_deref(), &token, |j| {
            j.receipts.entry(key.clone()).or_default().insert(token.clone(), receipt);
            j.bindings.insert(key, (project, version));
        })?;
        drop(state);
        let _ = self.wake.send(());
        Ok(Submission { project_id: project, version_id: version })
    }
    pub fn bind(&self, local_key: &str, project_id: Uuid, version_id: Uuid) -> Result<()> {
        if !valid_label(local_key) {
            return Err(Error("invalid local key".into()));
        }
        let mut s = lock(&self.shared);
        if self.stop.load(Ordering::Acquire) {
            return Err(Error("sync service has stopped".into()));
        }
        if !s.state.catalog.iter().any(|r| r.project_id == project_id && r.version_id == version_id) {
            return Err(Error("revision is not verified locally".into()));
        }
        let mut next = s.journal.clone();
        next.bindings.insert(local_key.into(), (project_id, version_id));
        journal(&self.cache, &next)?;
        s.journal = next;
        Ok(())
    }
    /// Creates a new immutable revision of `project_id` whose parent is `version_id`, with the same payload and preview and a new title.
    /// The payload must be cached (`request_payload` first). Local keys bound to `version_id` while it is a head move to the new revision.
    pub fn rename(&self, project_id: Uuid, version_id: Uuid, title: &str) -> Result<Submission> {
        if !valid_label(title) {
            return Err(Error("invalid revision title".into()));
        }
        if !lock(&self.shared).state.catalog.iter().any(|r| r.project_id == project_id && r.version_id == version_id) {
            return Err(Error("revision is not in the catalog".into()));
        }
        let old = load_manifest(&self.cache, version_id, &self.app)?;
        let native = bounded_file(&payload(&self.cache, version_id), MAX_PAYLOAD).ok().filter(|b| b.len() as u64 == old.size && hash(b) == old.sha256);
        let native = native.ok_or_else(|| Error(NOT_LOCAL.into()))?;
        let preview = old.preview.as_ref().and_then(|pr| read_preview(&self.cache, version_id, pr).map(|b| (pr.clone(), b)));
        let mut state = lock(&self.shared);
        if self.stop.load(Ordering::Acquire) {
            return Err(Error("sync service has stopped".into()));
        }
        let head = state.state.catalog.iter().any(|r| r.version_id == version_id && r.heads.contains(&version_id));
        let version = Uuid::new_v4();
        let m = Manifest {
            schema: 1,
            app: self.app.clone(),
            project: project_id,
            version,
            parents: vec![version_id],
            title: title.into(),
            extension: old.extension,
            size: old.size,
            sha256: old.sha256,
            created_ms: now_ms(),
            preview: preview.as_ref().map(|(pr, _)| pr.clone()),
        };
        let token = format!("rename:{version}");
        let bytes = preview.as_ref().map(|(_, b)| b.as_slice());
        enqueue(&self.cache, &mut state, m, &native, bytes, &token, |j| {
            if head {
                for (p, v) in j.bindings.values_mut() {
                    if *p == project_id && *v == version_id {
                        *v = version;
                    }
                }
            }
        })?;
        drop(state);
        let _ = self.wake.send(());
        Ok(Submission { project_id, version_id: version })
    }
    /// Asks the worker to download and verify a revision's payload. Answered by `SyncEvent::Fetched` or `SyncEvent::FetchFailed`;
    /// an already cached payload is answered immediately.
    pub fn request_payload(&self, version_id: Uuid) {
        let mut st = lock(&self.shared);
        let item = st.state.catalog.iter().find(|r| r.version_id == version_id).cloned();
        let Some(item) = item else {
            st.state.events.push(SyncEvent::FetchFailed { version_id, error: "revision is not in the catalog".into() });
            return;
        };
        if self.stop.load(Ordering::Acquire) {
            st.state.events.push(SyncEvent::FetchFailed { version_id, error: "sync service has stopped".into() });
        } else if fs::metadata(&item.local_path).is_ok_and(|f| f.len() == item.size) {
            touch(&mut st, [version_id]);
            st.state.events.push(SyncEvent::Fetched { project_id: item.project_id, version_id, path: item.local_path });
        } else {
            st.requests.insert(version_id);
            drop(st);
            let _ = self.wake.send(());
        }
    }
    /// Declares the revisions currently open in the editor; their payloads are never evicted. Replaces the previous set.
    pub fn set_pinned(&self, versions: BTreeSet<Uuid>) {
        let mut st = lock(&self.shared);
        let touched: Vec<_> = st.pinned.union(&versions).copied().collect();
        touch(&mut st, touched);
        st.pinned = versions;
    }
    /// Sets the payload cache limit in bytes and evicts least-recently-used payloads right away if needed.
    pub fn set_cache_limit(&self, bytes: u64) {
        let mut st = lock(&self.shared);
        st.state.cache_limit = bytes;
        evict(&self.cache, &mut st);
    }
}
impl Drop for SyncService {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn extension(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.bytes().all(|c| c.is_ascii_alphanumeric())
}
fn payload(c: &Path, v: Uuid) -> PathBuf {
    c.join("payloads").join(v.to_string())
}
fn preview_file(c: &Path, v: Uuid) -> PathBuf {
    c.join("previews").join(v.to_string())
}
fn bounded_file(p: &Path, max: u64) -> Result<Vec<u8>> {
    read_bounded(File::open(p)?, max)
}
fn read_bounded(r: impl Read, max: u64) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    r.take(max + 1).read_to_end(&mut b)?;
    if b.len() as u64 > max {
        return Err(Error("response exceeds size limit".into()));
    }
    Ok(b)
}
fn read_response(response: Response, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response.take(max + 1).read_to_end(&mut bytes).map_err(|_| Error(CONNECTION_UNAVAILABLE.into()))?;
    if bytes.len() as u64 > max {
        return Err(Error("response exceeds size limit".into()));
    }
    Ok(bytes)
}

struct Remote {
    cancel: Option<Arc<AtomicBool>>,
    client: Client,
    base: Url,
    user: String,
    password: String,
}
impl Remote {
    fn new(c: Config) -> Result<Self> {
        let mut u = Url::parse(&c.endpoint).map_err(|_| Error("invalid endpoint".into()))?;
        if !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || !(u.scheme() == "https" || (u.scheme() == "http" && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "::1"))))
        {
            return Err(Error("endpoint must use HTTPS without embedded credentials, query, or fragment".into()));
        }
        if !u.path().ends_with('/') {
            u.set_path(&format!("{}/", u.path()));
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error("HTTP client initialization failed".into()))?;
        Ok(Self { cancel: None, client, base: u, user: c.username, password: c.password })
    }
    fn url(&self, p: &str) -> Result<Url> {
        self.base.join(p).map_err(|_| Error("invalid remote path".into()))
    }
    fn send(&self, method: Method, p: &str, body: Option<Vec<u8>>, headers: &[(&str, String)]) -> Result<Response> {
        if self.cancel.as_ref().is_some_and(|stop| stop.load(Ordering::Acquire)) {
            return Err(Error(CANCELLED.into()));
        }
        let mut r = self.client.request(method, self.url(p)?).basic_auth(&self.user, Some(&self.password));
        if let Some(b) = body {
            r = r.body(b)
        }
        for (k, v) in headers {
            r = r.header(*k, v)
        }
        r.send().map_err(|_| Error(CONNECTION_UNAVAILABLE.into()))
    }
    fn get(&self, p: &str, max: u64) -> Result<Option<Vec<u8>>> {
        let r = self.send(Method::GET, p, None, &[])?;
        if r.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(Error(format!("WebDAV GET returned {}", r.status().as_u16())));
        }
        Ok(Some(read_response(r, max)?))
    }
    fn immutable(&self, p: &str, b: &[u8], max: u64) -> Result<()> {
        if let Some(existing) = self.get(p, max)? {
            return if existing == b { Ok(()) } else { Err(Error("immutable revision differs from existing remote object".into())) };
        }
        let temp = format!("{}.{}.upload", p, Uuid::new_v4());
        let r = self.send(Method::PUT, &temp, Some(b.to_vec()), &[])?;
        if !r.status().is_success() {
            return Err(Error(format!("WebDAV PUT returned {}", r.status().as_u16())));
        }
        let dest = self.url(p)?.to_string();
        let r = self.send(
            Method::from_bytes(b"MOVE").map_err(|_| Error("invalid method".into()))?,
            &temp,
            None,
            &[("Destination", dest), ("Overwrite", "F".into())],
        )?;
        if !r.status().is_success() && r.status() != StatusCode::PRECONDITION_FAILED {
            return Err(Error(format!("WebDAV MOVE returned {}", r.status().as_u16())));
        }
        match self.get(p, max)? {
            Some(v) if v == b => Ok(()),
            _ => Err(Error("remote publication verification failed".into())),
        }
    }
}
fn validate(m: &Manifest, app: &str) -> Result<()> {
    if m.schema != 1
        || m.app != app
        || !valid_label(&m.title)
        || !extension(&m.extension)
        || m.parents.len() > 32
        || m.parents.contains(&m.version)
        || m.size > MAX_PAYLOAD
        || m.sha256.len() != 64
        || !m.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || m.preview.as_ref().is_some_and(|p| p.size > MAX_PREVIEW || p.sha256.len() != 64 || !p.sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error("invalid revision manifest".into()));
    }
    Ok(())
}
fn load_manifest(c: &Path, v: Uuid, app: &str) -> Result<Manifest> {
    let m: Manifest = serde_json::from_slice(&bounded_file(&c.join("manifests").join(format!("{v}.json")), MAX_MANIFEST)?)?;
    validate(&m, app)?;
    if m.version != v {
        return Err(Error("manifest filename/version mismatch".into()));
    }
    Ok(m)
}
/// A cache written before the optional fields existed lacks values the remote manifest carries; anything else that differs is a violation.
fn upgrade(known: &Manifest, remote: &Manifest) -> bool {
    let mut k = known.clone();
    k.created_ms = k.created_ms.or(remote.created_ms);
    k.preview = k.preview.or_else(|| remote.preview.clone());
    k == *remote
}
fn preview_ok(b: &[u8], pr: &PreviewRef) -> bool {
    b.len() as u64 == pr.size && b.starts_with(PNG_SIGNATURE) && hash(b) == pr.sha256
}
fn read_preview(c: &Path, v: Uuid, pr: &PreviewRef) -> Option<Vec<u8>> {
    bounded_file(&preview_file(c, v), MAX_PREVIEW).ok().filter(|b| preview_ok(b, pr))
}
fn cycle(r: &Remote, c: &Path, app: &str, s: &Mutex<Shared>, stop: &AtomicBool) -> Result<()> {
    // Requested payloads are served even when publishing fails (offline), so a request never waits for the network to return.
    let published = publish(r, c, app, s, stop);
    serve(Some(r), c, app, s, stop);
    published?;
    sync(r, c, app, s, stop)?;
    serve(Some(r), c, app, s, stop);
    Ok(())
}
/// Uploads pending revisions: payload, then preview, then the manifest last (the manifest is the unit of visibility).
fn publish(r: &Remote, c: &Path, app: &str, s: &Mutex<Shared>, stop: &AtomicBool) -> Result<()> {
    // Flat UUID filenames avoid untrusted path interpretation and intermediate collection requirements.
    let pending = lock(s).journal.pending.clone();
    for mut p in pending {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        validate(&p.manifest, app)?;
        let v = p.manifest.version;
        let b = bounded_file(&payload(c, v), MAX_PAYLOAD)?;
        if b.len() as u64 != p.manifest.size || hash(&b) != p.manifest.sha256 {
            return Err(Error("local outbox checksum mismatch".into()));
        }
        let preview = p.manifest.preview.as_ref().and_then(|pr| read_preview(c, v, pr));
        if p.manifest.preview.is_some() && preview.is_none() {
            drop_preview(c, s, &mut p)?;
        }
        r.immutable(&format!("{v}.payload"), &b, MAX_PAYLOAD)?;
        if let Some(pv) = &preview {
            r.immutable(&format!("{v}.preview"), pv, MAX_PREVIEW)?;
        }
        r.immutable(&format!("{v}.json"), &serde_json::to_vec(&p.manifest)?, MAX_MANIFEST)?;
        let mut shared = lock(s);
        let mut next = shared.journal.clone();
        next.pending.retain(|x| x.manifest.version != v);
        journal(c, &next)?;
        shared.journal = next;
        shared.confirmed.insert(v);
        shared.state.events.push(SyncEvent::Published { project_id: p.manifest.project, version_id: v, local_token: p.local_token });
    }
    Ok(())
}
/// A pending revision whose local preview vanished is published without it rather than blocking the outbox.
fn drop_preview(c: &Path, s: &Mutex<Shared>, p: &mut Pending) -> Result<()> {
    let v = p.manifest.version;
    p.manifest.preview = None;
    atomic(&c.join("manifests").join(format!("{v}.json")), &serde_json::to_vec(&p.manifest)?)?;
    let mut shared = lock(s);
    let mut next = shared.journal.clone();
    if let Some(x) = next.pending.iter_mut().find(|x| x.manifest.version == v) {
        x.manifest = p.manifest.clone();
    }
    journal(c, &next)?;
    shared.journal = next;
    for item in shared.state.catalog.iter_mut().filter(|r| r.version_id == v) {
        item.preview_path = None;
    }
    shared.state.events.push(SyncEvent::Error(format!("revision {v} preview is missing or damaged locally; publishing it without the preview")));
    Ok(())
}
/// Lists the remote, validates manifests and previews, and rebuilds the catalog. Payloads are not downloaded here.
fn sync(r: &Remote, c: &Path, app: &str, s: &Mutex<Shared>, stop: &AtomicBool) -> Result<()> {
    let res = r.send(
        Method::from_bytes(b"PROPFIND").map_err(|_| Error("invalid method".into()))?,
        "",
        Some(b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/></d:prop></d:propfind>".to_vec()),
        &[("Depth", "1".into()), ("Content-Type", "application/xml".into())],
    )?;
    if !res.status().is_success() {
        return Err(Error(format!("WebDAV listing returned {}", res.status().as_u16())));
    }
    let list = read_response(res, MAX_LIST)?;
    let ids = list_ids(&list)?;
    let mut manifests = Vec::new();
    let mut fresh = BTreeSet::new();
    for id in ids {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(b) = r.get(&format!("{id}.json"), MAX_MANIFEST)? {
            let m: Manifest = serde_json::from_slice(&b)?;
            if m.app != app {
                continue;
            }
            validate(&m, app)?;
            if m.version != id {
                return Err(Error("manifest filename/version mismatch".into()));
            }
            let cached = c.join("manifests").join(format!("{}.json", m.version));
            if cached.exists() {
                let known: Manifest = serde_json::from_slice(&bounded_file(&cached, MAX_MANIFEST)?)?;
                if known != m {
                    if !upgrade(&known, &m) {
                        return Err(Error("published remote revision changed immutable metadata".into()));
                    }
                    fresh.insert(m.version);
                }
            } else {
                fresh.insert(m.version);
            }
            manifests.push(m);
        }
    }
    validate_dag(&manifests)?;
    for m in &manifests {
        for p in &m.parents {
            if manifests.iter().any(|v| v.version == *p && v.project != m.project) {
                return Err(Error("revision parent belongs to another project".into()));
            }
        }
    }
    let pending: BTreeSet<Uuid> = lock(s).journal.pending.iter().map(|p| p.manifest.version).collect();
    let mut catalog = Vec::new();
    for m in &manifests {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if fresh.contains(&m.version) {
            atomic(&c.join("manifests").join(format!("{}.json", m.version)), &serde_json::to_vec(m)?)?;
        }
        if let Some(pr) = &m.preview {
            fetch_preview(r, c, s, m.version, pr)?;
        }
        catalog.push(revision(c, m, pending.contains(&m.version)));
    }
    refresh_heads(&mut catalog);
    let mut shared = lock(s);
    let known: BTreeSet<Uuid> = shared.state.catalog.iter().map(|v| v.version_id).collect();
    for item in &catalog {
        if !known.contains(&item.version_id) {
            shared.state.events.push(SyncEvent::Downloaded(item.clone()));
        }
    }
    let listed: BTreeSet<Uuid> = catalog.iter().map(|v| v.version_id).collect();
    for item in &shared.state.catalog {
        if !listed.contains(&item.version_id) {
            catalog.push(item.clone());
        }
    }
    refresh_heads(&mut catalog);
    shared.state.catalog = catalog;
    shared.confirmed = listed;
    evict(c, &mut shared);
    Ok(())
}
/// Downloads a revision's preview unless it is already stored. A preview that fails verification is skipped and reported once;
/// only connection loss and cancellation abort the cycle.
fn fetch_preview(r: &Remote, c: &Path, s: &Mutex<Shared>, v: Uuid, pr: &PreviewRef) -> Result<()> {
    let path = preview_file(c, v);
    if fs::metadata(&path).is_ok_and(|f| f.len() == pr.size) || lock(s).bad_previews.contains(&v) {
        return Ok(());
    }
    let stored = match r.get(&format!("{v}.preview"), pr.size) {
        Err(e) if e.0 == CANCELLED || e.0 == CONNECTION_UNAVAILABLE => return Err(e),
        Ok(Some(b)) if preview_ok(&b, pr) => atomic(&path, &b).map_err(|e| e.0),
        Ok(Some(_)) => Err("size, checksum or format mismatch".to_owned()),
        Ok(None) => Err("object missing".to_owned()),
        Err(e) => Err(e.0),
    };
    if let Err(e) = stored {
        let mut shared = lock(s);
        if shared.bad_previews.insert(v) {
            shared.state.events.push(SyncEvent::Error(format!("revision {v} preview skipped: {e}")));
        }
    }
    Ok(())
}
/// Serves `request_payload` calls. Without a connection (or after it drops) cached payloads still succeed and the rest fail as offline.
fn serve(r: Option<&Remote>, c: &Path, app: &str, s: &Mutex<Shared>, stop: &AtomicBool) {
    let todo = std::mem::take(&mut lock(s).requests);
    let mut offline = r.is_none();
    let mut results = Vec::new();
    for v in todo {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let project = lock(s).state.catalog.iter().find(|x| x.version_id == v).map(|x| x.project_id);
        let res = match project {
            None => Err("revision is not in the catalog".to_owned()),
            Some(p) => match fetch_payload(r.filter(|_| !offline), c, app, v) {
                Ok(path) => Ok((p, path)),
                Err(e) if e.0 == CANCELLED => return,
                Err(e) if e.0 == CONNECTION_UNAVAILABLE => {
                    offline = true;
                    Err(OFFLINE_FETCH.to_owned())
                }
                Err(e) => Err(e.0),
            },
        };
        results.push((v, res));
    }
    if results.is_empty() {
        return;
    }
    let mut shared = lock(s);
    let mut fetched = BTreeSet::new();
    for (v, res) in results {
        match res {
            Ok((project_id, path)) => {
                fetched.insert(v);
                shared.state.events.push(SyncEvent::Fetched { project_id, version_id: v, path });
            }
            Err(error) => shared.state.events.push(SyncEvent::FetchFailed { version_id: v, error }),
        }
    }
    touch(&mut shared, fetched.iter().copied());
    evict(c, &mut shared);
}
fn fetch_payload(r: Option<&Remote>, c: &Path, app: &str, v: Uuid) -> Result<PathBuf> {
    let m = load_manifest(c, v, app)?;
    let path = payload(c, v);
    let sound = |b: &[u8]| b.len() as u64 == m.size && hash(b) == m.sha256;
    if bounded_file(&path, MAX_PAYLOAD).is_ok_and(|b| sound(&b)) {
        return Ok(path);
    }
    let r = r.ok_or_else(|| Error(OFFLINE_FETCH.into()))?;
    let b = r.get(&format!("{v}.payload"), m.size)?.ok_or_else(|| Error("published revision payload missing".into()))?;
    if !sound(&b) {
        return Err(Error("download checksum mismatch".into()));
    }
    atomic(&path, &b)?;
    Ok(path)
}
fn touch(st: &mut Shared, versions: impl IntoIterator<Item = Uuid>) {
    let now = now_ms().unwrap_or(0);
    for v in versions {
        st.journal.access.insert(v, now);
        st.dirty = true;
    }
}
fn payload_bytes(c: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(c.join("payloads")) else { return 0 };
    let named = entries.flatten().filter(|e| e.file_name().to_str().is_some_and(|n| Uuid::parse_str(n).is_ok()));
    named.filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).fold(0, |total: u64, m| total.saturating_add(m.len()))
}
/// Deletes least-recently-used payloads while the cache exceeds its limit. Never deletes pending revisions, pinned revisions,
/// heads of projects with several heads, revisions not confirmed on the remote, or payloads used within the last minute.
/// Manifests and previews are kept.
fn evict(c: &Path, st: &mut Shared) {
    let now = now_ms().unwrap_or(0);
    let known: BTreeSet<Uuid> = st.state.catalog.iter().map(|r| r.version_id).collect();
    let before = st.journal.access.len();
    st.journal.access.retain(|v, _| known.contains(v));
    st.dirty |= st.journal.access.len() != before;
    let mut used = payload_bytes(c);
    if used > st.state.cache_limit {
        let pending: BTreeSet<Uuid> = st.journal.pending.iter().map(|p| p.manifest.version).collect();
        let conflicts: BTreeSet<Uuid> = st.state.catalog.iter().filter(|r| r.heads.len() > 1).flat_map(|r| r.heads.iter().copied()).collect();
        let mut victims: Vec<_> = (st.state.catalog.iter())
            .filter(|r| r.cached && st.confirmed.contains(&r.version_id))
            .filter(|r| ![&pending, &st.pinned, &conflicts].iter().any(|set| set.contains(&r.version_id)))
            .filter(|r| st.journal.access.get(&r.version_id).is_none_or(|t| now.saturating_sub(*t) >= RECENT_MS))
            .map(|r| (st.journal.access.get(&r.version_id).copied().unwrap_or(0), r.created_ms, r.version_id, r.size))
            .collect();
        victims.sort();
        for (_, _, v, size) in victims {
            if used <= st.state.cache_limit {
                break;
            }
            if fs::remove_file(payload(c, v)).is_ok() {
                used = used.saturating_sub(size);
            }
        }
    }
    refresh_flags(c, st);
    st.state.cache_bytes = payload_bytes(c);
    if std::mem::take(&mut st.dirty) {
        let _ = journal(c, &st.journal);
    }
}
fn refresh_flags(c: &Path, st: &mut Shared) {
    let pending: BTreeSet<Uuid> = st.journal.pending.iter().map(|p| p.manifest.version).collect();
    for r in &mut st.state.catalog {
        r.pending = pending.contains(&r.version_id);
        r.cached = fs::metadata(payload(c, r.version_id)).is_ok_and(|f| f.len() == r.size);
    }
}
/// Event queue bound: when full, plain `Downloaded` notifications go first so fetch results are not lost behind a large first sync.
fn trim_events(events: &mut Vec<SyncEvent>) {
    let mut excess = events.len().saturating_sub(256);
    events.retain(|e| {
        let skip = excess > 0 && matches!(e, SyncEvent::Downloaded(_));
        excess -= usize::from(skip);
        !skip
    });
    let rest = events.len().saturating_sub(256);
    events.drain(..rest);
}
/// Stores a new local revision: payload, preview, manifest, then the journal, and makes it visible in the catalog.
fn enqueue(c: &Path, st: &mut Shared, m: Manifest, native: &[u8], preview: Option<&[u8]>, token: &str, edit: impl FnOnce(&mut Journal)) -> Result<()> {
    atomic(&payload(c, m.version), native)?;
    if let Some(p) = preview {
        atomic(&preview_file(c, m.version), p)?;
    }
    atomic(&c.join("manifests").join(format!("{}.json", m.version)), &serde_json::to_vec(&m)?)?;
    let mut next = st.journal.clone();
    next.pending.push(Pending { manifest: m.clone(), local_token: token.into() });
    next.access.insert(m.version, now_ms().unwrap_or(0));
    edit(&mut next);
    journal(c, &next)?;
    st.journal = next;
    st.state.cache_bytes = st.state.cache_bytes.saturating_add(native.len() as u64);
    st.state.catalog.push(revision(c, &m, true));
    refresh_heads(&mut st.state.catalog);
    st.state.events.push(SyncEvent::Durable { project_id: m.project, version_id: m.version, local_token: token.into() });
    Ok(())
}
fn list_ids(b: &[u8]) -> Result<BTreeSet<Uuid>> {
    use quick_xml::{events::Event, Reader};
    let mut reader = Reader::from_reader(b);
    reader.config_mut().trim_text(true);
    let mut in_href = false;
    let mut ids = BTreeSet::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                in_href = e.local_name().as_ref() == b"href";
            }
            Ok(Event::Text(e)) if in_href => {
                let t = e.unescape().map_err(|_| Error("invalid listing text".into()))?;
                if let Some(name) = t.rsplit('/').next() {
                    if let Some(stem) = name.strip_suffix(".json") {
                        if let Ok(id) = Uuid::parse_str(stem) {
                            ids.insert(id);
                            if ids.len() > 10000 {
                                return Err(Error("catalog exceeds revision limit".into()));
                            }
                        }
                    }
                }
            }
            Ok(Event::End(_)) => in_href = false,
            Ok(Event::Eof) => break,
            Err(_) => return Err(Error("invalid WebDAV XML".into())),
            _ => {}
        }
    }
    Ok(ids)
}

fn revision(c: &Path, m: &Manifest, pending: bool) -> DownloadedRevision {
    let local_path = payload(c, m.version);
    DownloadedRevision {
        project_id: m.project,
        version_id: m.version,
        parents: m.parents.clone(),
        heads: Vec::new(),
        title: m.title.clone(),
        extension: m.extension.clone(),
        created_ms: m.created_ms,
        size: m.size,
        preview_path: m.preview.as_ref().map(|_| preview_file(c, m.version)).filter(|p| p.is_file()),
        cached: fs::metadata(&local_path).is_ok_and(|f| f.len() == m.size),
        pending,
        local_path,
    }
}

fn build_catalog<'a>(c: &Path, manifests: impl Iterator<Item = &'a Manifest>, pending: &BTreeSet<Uuid>) -> Vec<DownloadedRevision> {
    let mut catalog: Vec<_> = manifests.map(|m| revision(c, m, pending.contains(&m.version))).collect();
    refresh_heads(&mut catalog);
    catalog
}

fn refresh_heads(catalog: &mut [DownloadedRevision]) {
    let parents: BTreeSet<_> = catalog.iter().flat_map(|r| r.parents.iter().copied()).collect();
    let mut heads: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for r in catalog.iter().filter(|r| !parents.contains(&r.version_id)) {
        heads.entry(r.project_id).or_default().push(r.version_id);
    }
    for item in catalog {
        item.heads = heads.get(&item.project_id).cloned().unwrap_or_default();
    }
}

fn validate_dag(manifests: &[Manifest]) -> Result<()> {
    let all: BTreeMap<_, _> = manifests.iter().map(|m| (m.version, m)).collect();
    let mut indegree = BTreeMap::new();
    let mut children: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for m in manifests {
        let unique: BTreeSet<_> = m.parents.iter().copied().collect();
        if unique.len() != m.parents.len() {
            return Err(Error("duplicate revision parents".into()));
        }
        indegree.insert(m.version, m.parents.len());
        for p in &m.parents {
            let parent = all.get(p).ok_or_else(|| Error("revision parent missing from catalog".into()))?;
            if parent.project != m.project {
                return Err(Error("revision parent belongs to another project".into()));
            }
            children.entry(*p).or_default().push(m.version);
        }
    }
    let mut ready: Vec<_> = indegree.iter().filter(|(_, n)| **n == 0).map(|(v, _)| *v).collect();
    let mut seen = 0;
    while let Some(v) = ready.pop() {
        seen += 1;
        if let Some(next) = children.get(&v) {
            for n in next {
                if let Some(count) = indegree.get_mut(n) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        ready.push(*n)
                    }
                }
            }
        }
    }
    if seen != manifests.len() {
        return Err(Error("revision graph contains a cycle".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snap(key: &str) -> Snapshot {
        Snapshot {
            local_key: key.into(),
            title: "Untitled".into(),
            extension: "pcraft".into(),
            local_token: Uuid::new_v4().to_string(),
            native: b"native bytes".to_vec(),
            preview: None,
        }
    }
    fn offline(c: &Path) -> SyncService {
        SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, c, "photocraft").unwrap()
    }
    #[test]
    fn durable_restart_and_branch_identity() {
        let c = tempfile::tempdir().unwrap();
        let mut s = offline(c.path());
        let first = s.submit(snap("unsaved")).unwrap();
        let second = s.submit(snap("unsaved")).unwrap();
        assert_eq!(first.project_id, second.project_id);
        s.shutdown();
        let s = offline(c.path());
        assert_eq!(s.poll().pending, 2);
        let cat = s.poll().catalog;
        assert_eq!(cat.len(), 2);
        assert!(cat.iter().find(|r| r.version_id == second.version_id).unwrap().parents.contains(&first.version_id));
        s.bind("opened", first.project_id, first.version_id).unwrap();
        let branch = s.submit(snap("opened")).unwrap();
        let cat = s.poll().catalog;
        assert_eq!(cat.iter().find(|r| r.version_id == branch.version_id).unwrap().parents, vec![first.version_id]);
    }
    #[test]
    fn submission_receipts_survive_restart() {
        let cache = tempfile::tempdir().unwrap();
        let mut service = offline(cache.path());
        let mut first = snap("doc");
        first.local_token = "stable-token".into();
        let submitted = service.submit(first).unwrap();
        let mut retry = snap("doc");
        retry.local_token = "stable-token".into();
        assert_eq!(service.submit(retry).unwrap(), submitted);
        assert_eq!(service.poll().pending, 1);
        service.shutdown();
        let service = offline(cache.path());
        let mut retry = snap("doc");
        retry.local_token = "stable-token".into();
        assert_eq!(service.submit(retry).unwrap(), submitted);
        assert_eq!(service.poll().pending, 1);
        let mut bad = snap("doc");
        bad.local_token = "stable-token".into();
        bad.native = b"different".to_vec();
        assert!(service.submit(bad).is_err());
    }
    #[test]
    fn connection_loss_is_offline_without_error_and_preserves_pending() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let c = tempfile::tempdir().unwrap();
        let service = SyncService::new(Config { endpoint, username: "u".into(), password: "p".into() }, c.path(), "photocraft").unwrap();
        service.submit(snap("offline")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = service.poll();
            assert!(!state.events.iter().any(|e| matches!(e, SyncEvent::Error(_))));
            if matches!(state.status, SyncStatus::Offline) {
                assert_eq!(state.pending, 1);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn protocol_failure_is_actionable_error() {
        let dav = Dav::new();
        lock(&dav.objects).insert(format!("{}.json", Uuid::new_v4()), b"invalid manifest".to_vec());
        let c = tempfile::tempdir().unwrap();
        let service = SyncService::new(Config { endpoint: dav.endpoint.clone(), username: "u".into(), password: "p".into() }, c.path(), "photocraft").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = service.poll();
            if matches!(state.status, SyncStatus::Error(_)) {
                assert!(state.events.iter().any(|e| matches!(e, SyncEvent::Error(_))));
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn hostile_metadata_and_endpoint() {
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let mut b = snap("k");
        b.extension = "../escape".into();
        assert!(s.submit(b).is_err());
        for endpoint in ["http://example.com/", "https://user:secret@example.com/", "https://example.com/?password=x"] {
            assert!(Remote::new(Config { endpoint: endpoint.into(), username: String::new(), password: String::new() }).is_err());
        }
        assert!(read_bounded(&b"12345"[..], 4).is_err());
    }
    #[test]
    fn listing_ignores_traversal_and_rejects_malformed() {
        let id = Uuid::new_v4();
        let xml = format!("<d:multistatus xmlns:d='DAV:'><d:href>/root/{id}.json</d:href><d:href>/../evil.json</d:href></d:multistatus>");
        assert_eq!(list_ids(xml.as_bytes()).unwrap(), BTreeSet::from([id]));
        assert!(list_ids(b"<a><broken").is_err());
        let other = Uuid::new_v4();
        let xml = format!(
            "<d:multistatus xmlns:d='DAV:'><d:href>/root/{id}.preview</d:href><d:href>/root/{id}.payload</d:href><d:href>/root/{other}.json</d:href><d:href>/root/{id}.preview.{other}.upload</d:href></d:multistatus>"
        );
        assert_eq!(list_ids(xml.as_bytes()).unwrap(), BTreeSet::from([other]));
    }
    #[test]
    fn cache_lock_corruption_and_account_scope() {
        let d = Dav::new();
        let c = tempfile::tempdir().unwrap();
        let mut s = offline(c.path());
        assert!(SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, c.path(), "photocraft").is_err());
        s.submit(snap("k")).unwrap();
        let handle = s.submitter();
        s.shutdown();
        assert!(handle.submit(snap("old")).is_err());
        let mut s = SyncService::new(Config { endpoint: d.endpoint.clone(), username: "one".into(), password: "p".into() }, c.path(), "photocraft").unwrap();
        s.shutdown();
        assert!(SyncService::new(Config { endpoint: d.endpoint.clone(), username: "two".into(), password: "p".into() }, c.path(), "photocraft").is_err());
        // The connected service may already have published `sub`; a pending revision keeps the strict startup check.
        let mut o = offline(c.path());
        let pending = o.submit(snap("k2")).unwrap();
        o.shutdown();
        fs::write(payload(c.path(), pending.version_id), b"damaged").unwrap();
        assert!(SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, c.path(), "photocraft").is_err());
    }
    #[test]
    fn interrupted_payload_retry_and_remote_immutability() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let sub = s.submit(snap("k")).unwrap();
        r.immutable(&format!("{}.payload", sub.version_id), b"native bytes", MAX_PAYLOAD).unwrap();
        assert!(!lock(&d.objects).contains_key(&format!("{}.json", sub.version_id)));
        cycle(&r, c.path(), "photocraft", &s.shared, &AtomicBool::new(false)).unwrap();
        let key = format!("{}.json", sub.version_id);
        let mut m: Manifest = serde_json::from_slice(lock(&d.objects).get(&key).unwrap()).unwrap();
        m.title = "modified".into();
        lock(&d.objects).insert(key, serde_json::to_vec(&m).unwrap());
        assert!(cycle(&r, c.path(), "photocraft", &s.shared, &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn concurrent_heads_preserved_and_cycles_rejected() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let first = s.submit(snap("k")).unwrap();
        cycle(&r, c.path(), "photocraft", &s.shared, &AtomicBool::new(false)).unwrap();
        let other = tempfile::tempdir().unwrap();
        let t = offline(other.path());
        cycle(&r, other.path(), "photocraft", &t.shared, &AtomicBool::new(false)).unwrap();
        t.bind("opened", first.project_id, first.version_id).unwrap();
        s.submit(snap("k")).unwrap();
        t.submit(snap("opened")).unwrap();
        cycle(&r, c.path(), "photocraft", &s.shared, &AtomicBool::new(false)).unwrap();
        cycle(&r, other.path(), "photocraft", &t.shared, &AtomicBool::new(false)).unwrap();
        assert!(t.poll().catalog.iter().all(|i| i.heads.len() == 2));
        let pending = lock(&s.shared).state.catalog.clone();
        assert!(!pending.is_empty());
        let mut a: Manifest = serde_json::from_slice(&fs::read(c.path().join("manifests").join(format!("{}.json", first.version_id))).unwrap()).unwrap();
        let mut b = a.clone();
        b.version = Uuid::new_v4();
        a.parents = vec![b.version];
        b.parents = vec![a.version];
        assert!(validate_dag(&[a, b]).is_err());
    }
    /// Run only against a disposable real WebDAV root; never prints credentials.
    #[test]
    #[ignore = "requires disposable real rclone WebDAV endpoint in CRAFT_SYNC_DAV_* environment"]
    fn real_rclone_two_clients_and_offline_restart() {
        fn config() -> Config {
            Config {
                endpoint: std::env::var("CRAFT_SYNC_DAV_URL").expect("test endpoint required"),
                username: std::env::var("CRAFT_SYNC_DAV_USERNAME").unwrap_or_default(),
                password: std::env::var("CRAFT_SYNC_DAV_PASSWORD").unwrap_or_default(),
            }
        }
        fn wait(service: &SyncService, predicate: impl Fn(&SyncState) -> bool) -> SyncState {
            let deadline = std::time::Instant::now() + Duration::from_secs(90);
            loop {
                service.request_sync();
                let state = service.poll();
                if predicate(&state) {
                    return state;
                }
                assert!(std::time::Instant::now() < deadline, "real DAV test timed out: {:?}", state.status);
                thread::sleep(Duration::from_millis(100));
            }
        }
        let app = format!("integration-{}", Uuid::new_v4());
        let ac = tempfile::tempdir().unwrap();
        let bc = tempfile::tempdir().unwrap();
        let oc = tempfile::tempdir().unwrap();
        let a = SyncService::new(config(), ac.path(), &app).unwrap();
        let b = SyncService::new(config(), bc.path(), &app).unwrap();
        let base = a.submit(snap("a")).unwrap();
        wait(&a, |s| s.pending == 0);
        wait(&b, |s| s.catalog.iter().any(|r| r.version_id == base.version_id));
        b.request_payload(base.version_id);
        let state = wait(&b, |s| s.catalog.iter().any(|r| r.version_id == base.version_id && r.cached));
        let downloaded = state.catalog.iter().find(|r| r.version_id == base.version_id).unwrap();
        assert_eq!(fs::read(&downloaded.local_path).unwrap(), b"native bytes");
        b.bind("b", base.project_id, base.version_id).unwrap();
        let left = a.submit(snap("a")).unwrap();
        let right = b.submit(snap("b")).unwrap();
        wait(&a, |s| s.pending == 0);
        wait(&b, |s| s.pending == 0);
        let state = wait(&b, |s| s.catalog.iter().any(|r| r.project_id == base.project_id && r.heads.len() == 2));
        let heads = &state.catalog.iter().find(|r| r.project_id == base.project_id).unwrap().heads;
        assert!(heads.contains(&left.version_id) && heads.contains(&right.version_id));
        let mut off = SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, oc.path(), &app).unwrap();
        let saved = off.submit(snap("offline")).unwrap();
        off.shutdown();
        let resumed = SyncService::new(config(), oc.path(), &app).unwrap();
        wait(&resumed, |s| s.pending == 0);
        wait(&b, |s| s.catalog.iter().any(|r| r.version_id == saved.version_id));
        b.request_payload(saved.version_id);
        let state = wait(&b, |s| s.catalog.iter().any(|r| r.version_id == saved.version_id && r.cached));
        let item = state.catalog.iter().find(|r| r.version_id == saved.version_id).unwrap();
        assert_eq!(fs::read(&item.local_path).unwrap(), b"native bytes");
    }
    // A real HTTP socket exercises request framing and the immutable no-overwrite precondition.
    struct Dav {
        endpoint: String,
        objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Dav {
        fn new() -> Self {
            use std::net::TcpListener;
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            let objects = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
            let o = objects.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let st = stop.clone();
            let thread = thread::spawn(move || {
                while !st.load(Ordering::Acquire) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut header = Vec::new();
                    let mut byte = [0];
                    while !header.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        header.push(byte[0]);
                    }
                    let h = String::from_utf8_lossy(&header);
                    let mut lines = h.lines();
                    let mut parts = lines.next().unwrap_or("").split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("/").trim_start_matches('/').to_owned();
                    let headers: BTreeMap<_, _> = lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.to_lowercase(), v.trim().to_owned()))).collect();
                    let len = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                    let mut body = vec![0; len];
                    let _ = stream.read_exact(&mut body);
                    let mut objects = lock(&o);
                    let (code, response) = match method {
                        "GET" => objects.get(&path).map(|b| (200, b.clone())).unwrap_or((404, vec![])),
                        "PUT" => {
                            objects.insert(path, body);
                            (201, vec![])
                        }
                        "MOVE" => {
                            let dest = Url::parse(headers.get("destination").unwrap()).unwrap().path().trim_start_matches('/').to_owned();
                            if objects.contains_key(&dest) {
                                (412, vec![])
                            } else {
                                let b = objects.remove(&path).unwrap();
                                objects.insert(dest, b);
                                (201, vec![])
                            }
                        }
                        "PROPFIND" => {
                            let body = format!(
                                "<d:multistatus xmlns:d='DAV:'>{}</d:multistatus>",
                                objects.keys().map(|p| format!("<d:response><d:href>/{p}</d:href></d:response>")).collect::<String>()
                            );
                            (207, body.into_bytes())
                        }
                        _ => (405, vec![]),
                    };
                    let _ = write!(stream, "HTTP/1.1 {code} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
                    let _ = stream.write_all(&response);
                }
            });
            Self { endpoint, objects, stop, thread: Some(thread) }
        }
        fn remote(&self) -> Remote {
            Remote::new(Config { endpoint: self.endpoint.clone(), username: "u".into(), password: "secret".into() }).unwrap()
        }
    }
    impl Drop for Dav {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }
    #[test]
    fn immutable_retry_and_verified_catalog() {
        let d = Dav::new();
        let r = d.remote();
        r.immutable("a.payload", b"first", MAX_PAYLOAD).unwrap();
        r.immutable("a.payload", b"first", MAX_PAYLOAD).unwrap();
        assert!(r.immutable("a.payload", b"changed", MAX_PAYLOAD).is_err());
        assert_eq!(lock(&d.objects).get("a.payload").unwrap(), b"first");
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let sub = s.submit(snap("k")).unwrap();
        cycle(&r, c.path(), "photocraft", &s.shared, &AtomicBool::new(false)).unwrap();
        assert_eq!(s.poll().pending, 0);
        assert!(lock(&d.objects).contains_key(&format!("{}.json", sub.version_id)));
        let target = tempfile::tempdir().unwrap();
        let other = offline(target.path());
        cycle(&r, target.path(), "photocraft", &other.shared, &AtomicBool::new(false)).unwrap();
        let cat = other.poll().catalog;
        assert!(!cat[0].cached && !cat[0].pending && !cat[0].local_path.exists());
        settle(&other);
        let events = fetch(&r, target.path(), &other, sub.version_id);
        assert!(matches!(&events[..], [SyncEvent::Fetched { version_id, .. }] if *version_id == sub.version_id));
        assert_eq!(fs::read(&cat[0].local_path).unwrap(), b"native bytes");
        assert!(other.poll().catalog[0].cached);
        fs::remove_file(&cat[0].local_path).unwrap();
        lock(&d.objects).insert(format!("{}.payload", sub.version_id), b"corrupt".to_vec());
        let events = fetch(&r, target.path(), &other, sub.version_id);
        assert!(matches!(&events[..], [SyncEvent::FetchFailed { error, .. }] if error.contains("checksum")));
        assert!(!cat[0].local_path.exists());
        assert!(!other.poll().catalog[0].cached);
    }
    // Helpers for the lazy-payload tests. `settle` waits for an offline service's first worker pass, so requests
    // inserted afterwards are served by the test and not by the background thread.
    fn png(extra: &[u8]) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend_from_slice(extra);
        v
    }
    fn snap_with(key: &str, native: Vec<u8>, preview: Option<Vec<u8>>) -> Snapshot {
        Snapshot { native, preview, ..snap(key) }
    }
    fn run(r: &Remote, c: &Path, s: &SyncService) {
        cycle(r, c, "photocraft", &s.shared, &AtomicBool::new(false)).unwrap();
    }
    fn settle(s: &SyncService) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !matches!(s.poll().status, SyncStatus::Offline) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn fetch(r: &Remote, c: &Path, s: &SyncService, v: Uuid) -> Vec<SyncEvent> {
        lock(&s.shared).requests.insert(v);
        serve(Some(r), c, "photocraft", &s.shared, &AtomicBool::new(false));
        s.poll().events
    }
    fn await_event<T>(s: &SyncService, seen: &mut Vec<SyncEvent>, pick: impl Fn(&SyncEvent) -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            seen.extend(s.poll().events);
            if let Some(found) = seen.iter().find_map(&pick) {
                return found;
            }
            assert!(std::time::Instant::now() < deadline, "timed out; saw {seen:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn downloaded(v: Uuid) -> impl Fn(&SyncEvent) -> Option<DownloadedRevision> {
        move |e| if let SyncEvent::Downloaded(r) = e { Some(r.clone()).filter(|r| r.version_id == v) } else { None }
    }
    fn fetched(v: Uuid) -> impl Fn(&SyncEvent) -> Option<(Uuid, PathBuf)> {
        move |e| if let SyncEvent::Fetched { project_id, version_id, path } = e { Some((*project_id, path.clone())).filter(|_| *version_id == v) } else { None }
    }
    fn failed(v: Option<Uuid>) -> impl Fn(&SyncEvent) -> Option<String> {
        move |e| if let SyncEvent::FetchFailed { version_id, error } = e { Some(error.clone()).filter(|_| v.is_none_or(|v| v == *version_id)) } else { None }
    }
    fn connected(d: &Dav, c: &Path) -> SyncService {
        SyncService::new(Config { endpoint: d.endpoint.clone(), username: "u".into(), password: "p".into() }, c, "photocraft").unwrap()
    }
    fn errors(events: &[SyncEvent]) -> usize {
        events.iter().filter(|e| matches!(e, SyncEvent::Error(_))).count()
    }
    fn rewrite_manifest(c: &Path, v: Uuid, edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) {
        let path = c.join("manifests").join(format!("{v}.json"));
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(value.as_object_mut().unwrap());
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    #[test]
    fn preview_round_trip_and_payload_on_demand() {
        let d = Dav::new();
        let r = d.remote();
        let ca = tempfile::tempdir().unwrap();
        let a = offline(ca.path());
        let thumb = png(b"thumbnail pixels");
        let sub = a.submit(snap_with("doc", b"native bytes".to_vec(), Some(thumb.clone()))).unwrap();
        let v = sub.version_id;
        assert_eq!(fs::read(ca.path().join("previews").join(v.to_string())).unwrap(), thumb);
        run(&r, ca.path(), &a);
        assert_eq!(a.poll().pending, 0);
        assert_eq!(lock(&d.objects).get(&format!("{v}.preview")).unwrap(), &thumb);
        assert!(lock(&d.objects).contains_key(&format!("{v}.payload")) && lock(&d.objects).contains_key(&format!("{v}.json")));
        let remote: Manifest = serde_json::from_slice(lock(&d.objects).get(&format!("{v}.json")).unwrap()).unwrap();
        assert_eq!(remote.preview, Some(PreviewRef { size: thumb.len() as u64, sha256: hash(&thumb) }));
        assert!(remote.created_ms.is_some());
        let own = a.poll().catalog;
        assert!(own[0].cached && !own[0].pending && own[0].preview_path.is_some());
        let cb = tempfile::tempdir().unwrap();
        let b = connected(&d, cb.path());
        let mut seen = Vec::new();
        let rev = await_event(&b, &mut seen, downloaded(v));
        assert_eq!(fs::read(rev.preview_path.as_ref().unwrap()).unwrap(), thumb);
        assert!(!rev.cached && !rev.local_path.exists() && !rev.pending);
        assert_eq!((rev.size, rev.title.as_str(), rev.created_ms), (12, "Untitled", remote.created_ms));
        assert!(!b.poll().catalog[0].cached);
        b.request_payload(v);
        let (project_id, path) = await_event(&b, &mut seen, fetched(v));
        assert_eq!((fs::read(&path).unwrap(), project_id, path), (b"native bytes".to_vec(), sub.project_id, rev.local_path.clone()));
        let after = b.poll();
        assert!(after.catalog[0].cached && after.cache_bytes == 12 && after.cache_limit == DEFAULT_CACHE_LIMIT);
        // An already cached payload is answered immediately, on the caller thread.
        b.request_payload(v);
        assert!(matches!(&b.poll().events[..], [SyncEvent::Fetched { .. }]));
        assert_eq!(errors(&seen), 0);
    }
    #[test]
    fn invalid_previews_are_rejected_and_valid_ones_stored() {
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        assert!(s.submit(snap_with("k", b"x".to_vec(), Some(b"GIF89a not a png".to_vec()))).is_err());
        assert!(s.submit(snap_with("k", b"x".to_vec(), Some(Vec::new()))).is_err());
        let mut huge = png(b"");
        huge.resize(MAX_PREVIEW as usize + 1, 0);
        assert!(s.submit(snap_with("k", b"x".to_vec(), Some(huge.clone()))).is_err());
        assert_eq!(s.poll().pending, 0);
        assert_eq!(fs::read_dir(c.path().join("previews")).unwrap().count(), 0);
        huge.truncate(MAX_PREVIEW as usize);
        let ok = s.submit(snap_with("k", b"x".to_vec(), Some(huge.clone()))).unwrap();
        assert_eq!(fs::read(c.path().join("previews").join(ok.version_id.to_string())).unwrap(), huge);
        let m = load_manifest(c.path(), ok.version_id, "photocraft").unwrap();
        assert_eq!(m.preview, Some(PreviewRef { size: MAX_PREVIEW, sha256: hash(&huge) }));
        let mut bad = m.clone();
        bad.preview = Some(PreviewRef { size: MAX_PREVIEW + 1, sha256: hash(b"x") });
        assert!(validate(&bad, "photocraft").is_err());
        bad.preview = Some(PreviewRef { size: 8, sha256: "xyz".into() });
        assert!(validate(&bad, "photocraft").is_err());
        assert!(validate(&m, "photocraft").is_ok());
    }
    #[test]
    fn tampered_remote_preview_is_skipped_and_reported_once() {
        let d = Dav::new();
        let r = d.remote();
        let ca = tempfile::tempdir().unwrap();
        let a = offline(ca.path());
        let versions: Vec<_> = (0..4).map(|n| a.submit(snap_with(&format!("k{n}"), vec![n], Some(png(&[n])))).unwrap().version_id).collect();
        run(&r, ca.path(), &a);
        lock(&d.objects).insert(format!("{}.preview", versions[0]), png(b"other"));
        lock(&d.objects).remove(&format!("{}.preview", versions[1]));
        let mut oversized = png(b"");
        oversized.resize(MAX_PREVIEW as usize + 10, 7);
        lock(&d.objects).insert(format!("{}.preview", versions[2]), oversized);
        let cb = tempfile::tempdir().unwrap();
        let b = offline(cb.path());
        for _ in 0..3 {
            run(&r, cb.path(), &b);
        }
        let state = b.poll();
        assert_eq!(state.catalog.len(), 4);
        assert_eq!(errors(&state.events), 3);
        for (i, v) in versions.iter().enumerate() {
            let item = state.catalog.iter().find(|r| r.version_id == *v).unwrap();
            assert_eq!(item.preview_path.is_some(), i == 3, "revision {i}");
        }
        assert!(state.events.iter().all(|e| !matches!(e, SyncEvent::Error(m) if !m.contains("preview skipped"))));
        assert!(!cb.path().join("previews").join(versions[0].to_string()).exists());
        // The catalog and later cycles keep working; the good preview is intact.
        run(&r, cb.path(), &b);
        assert_eq!(errors(&b.poll().events), 0);
    }
    #[test]
    fn pending_revision_with_missing_preview_publishes_without_it() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let v = s.submit(snap_with("k", b"x".to_vec(), Some(png(b"p")))).unwrap().version_id;
        fs::remove_file(c.path().join("previews").join(v.to_string())).unwrap();
        run(&r, c.path(), &s);
        assert!(!lock(&d.objects).contains_key(&format!("{v}.preview")));
        let remote: Manifest = serde_json::from_slice(lock(&d.objects).get(&format!("{v}.json")).unwrap()).unwrap();
        assert!(remote.preview.is_none());
        let state = s.poll();
        assert_eq!(state.pending, 0);
        assert!(state.catalog[0].preview_path.is_none() && state.events.iter().any(|e| matches!(e, SyncEvent::Published { .. })));
        assert!(load_manifest(c.path(), v, "photocraft").unwrap().preview.is_none());
    }
    #[test]
    fn old_cache_without_optional_fields_is_upgraded_but_real_changes_still_fail() {
        let d = Dav::new();
        let r = d.remote();
        let ca = tempfile::tempdir().unwrap();
        let a = offline(ca.path());
        let v = a.submit(snap_with("k", b"x".to_vec(), Some(png(b"p")))).unwrap().version_id;
        run(&r, ca.path(), &a);
        let cb = tempfile::tempdir().unwrap();
        let b = offline(cb.path());
        run(&r, cb.path(), &b);
        let full = load_manifest(cb.path(), v, "photocraft").unwrap();
        assert!(full.created_ms.is_some() && full.preview.is_some());
        // A cache written by an older client dropped the unknown fields.
        let strip = |m: &mut serde_json::Map<String, serde_json::Value>| {
            m.remove("created_ms");
            m.remove("preview");
        };
        rewrite_manifest(cb.path(), v, strip);
        let old = load_manifest(cb.path(), v, "photocraft").unwrap();
        assert!(old.created_ms.is_none() && old.preview.is_none());
        run(&r, cb.path(), &b);
        assert!(load_manifest(cb.path(), v, "photocraft").unwrap() == full);
        assert!(b.poll().catalog[0].preview_path.is_some());
        // Anything else that differs stays an error, with or without the missing optional fields.
        rewrite_manifest(cb.path(), v, |m| {
            strip(m);
            m.insert("title".into(), "changed".into());
        });
        assert!(cycle(&r, cb.path(), "photocraft", &b.shared, &AtomicBool::new(false)).unwrap_err().0.contains("immutable"));
        rewrite_manifest(cb.path(), v, |m| {
            m.insert("created_ms".into(), 1.into());
        });
        assert!(cycle(&r, cb.path(), "photocraft", &b.shared, &AtomicBool::new(false)).is_err());
        // The cache must never claim fields the remote lacks.
        let mut remote: serde_json::Value = serde_json::from_slice(lock(&d.objects).get(&format!("{v}.json")).unwrap()).unwrap();
        remote.as_object_mut().map(|m| m.remove("created_ms"));
        lock(&d.objects).insert(format!("{v}.json"), serde_json::to_vec(&remote).unwrap());
        rewrite_manifest(cb.path(), v, |m| {
            m.insert("created_ms".into(), full.created_ms.unwrap().into());
        });
        assert!(cycle(&r, cb.path(), "photocraft", &b.shared, &AtomicBool::new(false)).is_err());
        assert!(upgrade(&old, &full) && !upgrade(&full, &old) && upgrade(&full, &full));
    }
    #[test]
    fn startup_tolerates_missing_or_corrupt_published_payloads_but_not_pending_ones() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let mut a = offline(c.path());
        let missing = a.submit(snap("missing")).unwrap().version_id;
        let corrupt = a.submit(snap("corrupt")).unwrap().version_id;
        let intact = a.submit(snap("intact")).unwrap().version_id;
        let previewed = a.submit(snap_with("previewed", b"native bytes".to_vec(), Some(png(b"p")))).unwrap().version_id;
        run(&r, c.path(), &a);
        a.shutdown();
        fs::remove_file(payload(c.path(), missing)).unwrap();
        fs::write(payload(c.path(), corrupt), b"NATIVE BYTES").unwrap();
        fs::write(c.path().join("previews").join(previewed.to_string()), png(b"q")).unwrap();
        let mut a = offline(c.path());
        let state = a.poll();
        let by = |v: Uuid| state.catalog.iter().find(|r| r.version_id == v).unwrap();
        assert!(!by(missing).cached && !by(corrupt).cached && by(intact).cached && !by(missing).pending);
        assert!(!payload(c.path(), corrupt).exists() && payload(c.path(), intact).exists());
        assert!(by(previewed).preview_path.is_none() && !c.path().join("previews").join(previewed.to_string()).exists());
        assert_eq!(state.cache_bytes, 24);
        a.shutdown();
        // Pending revisions still need a valid payload.
        let mut a = offline(c.path());
        let pending = a.submit(snap("pending")).unwrap().version_id;
        a.shutdown();
        fs::remove_file(payload(c.path(), pending)).unwrap();
        let config = || Config { endpoint: String::new(), username: String::new(), password: String::new() };
        assert!(SyncService::new(config(), c.path(), "photocraft").is_err());
        fs::write(payload(c.path(), pending), b"native bytez").unwrap();
        assert!(SyncService::new(config(), c.path(), "photocraft").is_err());
    }
    #[test]
    fn eviction_is_lru_and_never_removes_protected_revisions() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let mut a = offline(c.path());
        let big = |n: u8| vec![n; 100];
        let loose: Vec<_> = (1..=4).map(|n| a.submit(snap_with(&format!("loose{n}"), big(n), None)).unwrap()).collect();
        let base = a.submit(snap_with("conflict", big(10), None)).unwrap();
        run(&r, c.path(), &a);
        a.bind("conflict2", base.project_id, base.version_id).unwrap();
        let left = a.submit(snap_with("conflict", big(11), None)).unwrap();
        let right = a.submit(snap_with("conflict2", big(12), None)).unwrap();
        run(&r, c.path(), &a);
        let pending = a.submit(snap_with("pending", big(13), None)).unwrap();
        let (r1, r2, r3, r4) = (loose[0].version_id, loose[1].version_id, loose[2].version_id, loose[3].version_id);
        {
            let mut st = lock(&a.shared);
            assert!(st.confirmed.contains(&r1) && !st.confirmed.contains(&pending.version_id));
            let times = [(r1, 10), (r2, 20), (r3, 30), (r4, 40), (base.version_id, 5), (left.version_id, 1), (right.version_id, 1), (pending.version_id, 1)];
            for (v, t) in times {
                st.journal.access.insert(v, t);
            }
        }
        let cached = |a: &SyncService| -> BTreeSet<Uuid> { a.poll().catalog.iter().filter(|r| r.cached).map(|r| r.version_id).collect() };
        let state = a.poll();
        assert_eq!((state.cache_bytes, state.cache_limit), (800, DEFAULT_CACHE_LIMIT));
        assert_eq!(state.catalog.iter().find(|r| r.version_id == left.version_id).unwrap().heads.len(), 2);
        let manifests = fs::read_dir(c.path().join("manifests")).unwrap().count();
        a.set_pinned(BTreeSet::from([r2]));
        // 800 -> 500 needs three payloads, oldest first: the conflict ancestor, then r1, then r3. r2 is pinned.
        a.set_cache_limit(500);
        let kept = cached(&a);
        assert!(!kept.contains(&base.version_id) && !kept.contains(&r1) && !kept.contains(&r3), "{kept:?}");
        assert!(kept.contains(&r2) && kept.contains(&r4) && kept.contains(&left.version_id) && kept.contains(&right.version_id));
        assert!(kept.contains(&pending.version_id));
        let state = a.poll();
        assert_eq!((state.cache_bytes, state.cache_limit), (500, 500));
        assert!(!payload(c.path(), r1).exists() && payload(c.path(), r2).exists());
        // Everything evictable goes at limit 0; the rest stays over the limit without errors.
        a.set_cache_limit(0);
        assert_eq!(cached(&a), BTreeSet::from([r2, left.version_id, right.version_id, pending.version_id]));
        let state = a.poll();
        assert_eq!(state.cache_bytes, 400);
        assert_eq!(errors(&state.events), 0);
        // Unpinning counts as a use: the payload is kept for a minute, then it is the first candidate again.
        a.set_pinned(BTreeSet::new());
        a.set_cache_limit(0);
        assert!(cached(&a).contains(&r2));
        lock(&a.shared).journal.access.insert(r2, 50);
        a.set_cache_limit(0);
        assert_eq!(cached(&a), BTreeSet::from([left.version_id, right.version_id, pending.version_id]));
        // Manifests, previews and the pending journal are never touched.
        assert_eq!(fs::read_dir(c.path().join("manifests")).unwrap().count(), manifests);
        assert_eq!(a.poll().pending, 1);
        assert!(a.poll().catalog.iter().find(|r| r.version_id == r1).map(|r| r.size == 100 && !r.cached).unwrap());
        // The access times persist.
        let saved: Journal = serde_json::from_slice(&fs::read(c.path().join("journal.json")).unwrap()).unwrap();
        assert!(saved.access.contains_key(&r2) && !saved.access.is_empty());
        // The pending revision publishes, and the evicted ones can be fetched again from the remote.
        run(&r, c.path(), &a);
        a.shutdown();
        let events = fetch(&r, c.path(), &a, r1);
        assert!(matches!(events.last(), Some(SyncEvent::Fetched { .. })));
        assert_eq!(fs::read(payload(c.path(), r1)).unwrap(), big(1));
    }
    #[test]
    fn unconfirmed_revisions_are_not_evicted() {
        let d = Dav::new();
        let r = d.remote();
        let ca = tempfile::tempdir().unwrap();
        let a = offline(ca.path());
        let v = a.submit(snap_with("k", vec![1; 100], None)).unwrap().version_id;
        let other = a.submit(snap_with("j", vec![2; 100], None)).unwrap().version_id;
        for version in [v, other] {
            lock(&a.shared).journal.access.insert(version, 1);
        }
        // Before any publication nothing is confirmed, so nothing may go even though the limit is exceeded.
        a.set_cache_limit(0);
        assert_eq!(a.poll().cache_bytes, 200);
        a.set_cache_limit(u64::MAX);
        run(&r, ca.path(), &a);
        // Remote deletion is not authoritative: a revision that vanished from the listing keeps its only copy.
        lock(&d.objects).remove(&format!("{v}.json"));
        run(&r, ca.path(), &a);
        a.set_cache_limit(0);
        let state = a.poll();
        assert!(state.catalog.iter().find(|r| r.version_id == v).unwrap().cached);
        assert!(!state.catalog.iter().find(|r| r.version_id == other).unwrap().cached);
    }
    #[test]
    fn recently_used_payloads_are_kept_for_a_minute() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let a = offline(c.path());
        let v = a.submit(snap_with("k", vec![1; 100], None)).unwrap().version_id;
        run(&r, c.path(), &a);
        // Just submitted and published: it is about to be opened or pinned, so a tiny limit must not delete it yet.
        a.set_cache_limit(0);
        assert!(a.poll().catalog[0].cached && a.poll().cache_bytes == 100);
        let now = now_ms().unwrap();
        lock(&a.shared).journal.access.insert(v, now - RECENT_MS + 5_000);
        a.set_cache_limit(0);
        assert!(a.poll().catalog[0].cached);
        lock(&a.shared).journal.access.insert(v, now - RECENT_MS - 1);
        a.set_cache_limit(0);
        assert!(!a.poll().catalog[0].cached && a.poll().cache_bytes == 0);
    }
    #[test]
    fn rename_creates_child_revision_with_same_content() {
        let d = Dav::new();
        let r = d.remote();
        let c = tempfile::tempdir().unwrap();
        let a = offline(c.path());
        let thumb = png(b"thumb");
        let first = a.submit(snap_with("doc", b"native bytes".to_vec(), Some(thumb.clone()))).unwrap();
        run(&r, c.path(), &a);
        let old_json = lock(&d.objects).get(&format!("{}.json", first.version_id)).unwrap().clone();
        assert!(a.rename(first.project_id, first.version_id, "").is_err());
        assert!(a.rename(first.project_id, first.version_id, "bad\ntitle").is_err());
        assert!(a.rename(first.project_id, Uuid::new_v4(), "x").is_err());
        assert!(a.rename(Uuid::new_v4(), first.version_id, "x").is_err());
        let renamed = a.rename(first.project_id, first.version_id, "Better name").unwrap();
        assert_eq!(renamed.project_id, first.project_id);
        assert_ne!(renamed.version_id, first.version_id);
        let state = a.poll();
        let item = state.catalog.iter().find(|r| r.version_id == renamed.version_id).unwrap();
        assert_eq!(
            (item.title.as_str(), item.parents.clone(), item.pending, item.cached, item.heads.clone()),
            ("Better name", vec![first.version_id], true, true, vec![renamed.version_id])
        );
        assert_eq!(fs::read(item.preview_path.as_ref().unwrap()).unwrap(), thumb);
        assert_eq!(state.pending, 1);
        let (old, new) = (load_manifest(c.path(), first.version_id, "photocraft").unwrap(), load_manifest(c.path(), renamed.version_id, "photocraft").unwrap());
        assert_eq!(
            (new.sha256.clone(), new.size, new.extension.clone(), new.preview.clone()),
            (old.sha256.clone(), old.size, old.extension.clone(), old.preview.clone())
        );
        assert!(new.created_ms >= old.created_ms && new.title == "Better name" && old.title == "Untitled");
        run(&r, c.path(), &a);
        assert_eq!(a.poll().pending, 0);
        assert_eq!(lock(&d.objects).get(&format!("{}.json", first.version_id)).unwrap(), &old_json);
        for ext in ["json", "payload", "preview"] {
            assert!(lock(&d.objects).contains_key(&format!("{}.{ext}", renamed.version_id)), "{ext}");
        }
        assert_eq!(lock(&d.objects).get(&format!("{}.payload", renamed.version_id)).unwrap(), b"native bytes");
        // The editor's key follows the renamed head, so its next autosave continues the chain instead of forking it.
        let next = a.submit(snap("doc")).unwrap();
        let state = a.poll();
        let item = state.catalog.iter().find(|r| r.version_id == next.version_id).unwrap();
        assert_eq!((item.parents.clone(), item.heads.clone()), (vec![renamed.version_id], vec![next.version_id]));
        // Renaming an older revision branches by design.
        a.rename(first.project_id, first.version_id, "Branch").unwrap();
        assert_eq!(a.poll().catalog[0].heads.len(), 2);
    }
    #[test]
    fn rename_requires_the_payload_locally() {
        let d = Dav::new();
        let r = d.remote();
        let ca = tempfile::tempdir().unwrap();
        let a = offline(ca.path());
        let sub = a.submit(snap("k")).unwrap();
        run(&r, ca.path(), &a);
        let cb = tempfile::tempdir().unwrap();
        let b = offline(cb.path());
        run(&r, cb.path(), &b);
        settle(&b);
        let err = b.rename(sub.project_id, sub.version_id, "New").unwrap_err();
        assert_eq!(err.0, "revision payload is not available locally; request it first");
        assert_eq!(b.poll().pending, 0);
        assert_eq!(b.poll().catalog.len(), 1);
        assert!(matches!(&fetch(&r, cb.path(), &b, sub.version_id)[..], [SyncEvent::Fetched { .. }]));
        let renamed = b.rename(sub.project_id, sub.version_id, "New").unwrap();
        assert_eq!(b.poll().pending, 1);
        run(&r, cb.path(), &b);
        assert!(lock(&d.objects).contains_key(&format!("{}.json", renamed.version_id)));
        // A damaged cached payload is not renamed either.
        fs::write(payload(cb.path(), sub.version_id), b"native bytez").unwrap();
        assert!(b.rename(sub.project_id, sub.version_id, "Again").is_err());
    }
    #[test]
    fn payload_request_while_offline_fails_cleanly() {
        let d = Dav::new();
        let r = d.remote();
        let ca = tempfile::tempdir().unwrap();
        let a = offline(ca.path());
        let sub = a.submit(snap("k")).unwrap();
        run(&r, ca.path(), &a);
        let cb = tempfile::tempdir().unwrap();
        {
            let b = offline(cb.path());
            run(&r, cb.path(), &b);
            let mut seen = Vec::new();
            b.request_payload(Uuid::new_v4());
            assert!(matches!(&b.poll().events.last(), Some(SyncEvent::FetchFailed { error, .. }) if error.contains("not in the catalog")));
            b.request_payload(sub.version_id);
            let error = await_event(&b, &mut seen, failed(Some(sub.version_id)));
            assert!(error.starts_with("offline"), "{error}");
            // The request was dropped, not queued forever.
            assert!(lock(&b.shared).requests.is_empty());
        }
        // A configured but unreachable server is also just an offline failure, never an error or panic.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let b = SyncService::new(Config { endpoint, username: "u".into(), password: "p".into() }, cb.path(), "photocraft").unwrap();
        let mut seen = Vec::new();
        b.request_payload(sub.version_id);
        let error = await_event(&b, &mut seen, failed(None));
        assert!(error.starts_with("offline"), "{error}");
        assert_eq!(errors(&seen), 0);
        assert!(!b.poll().catalog[0].cached);
        // A revision cached locally is still available offline.
        a.request_payload(sub.version_id);
        assert!(matches!(&a.poll().events.last(), Some(SyncEvent::Fetched { path, .. }) if path == &payload(ca.path(), sub.version_id)));
    }
    #[test]
    fn journal_written_before_previews_and_access_tracking_still_loads() {
        let c = tempfile::tempdir().unwrap();
        let mut s = offline(c.path());
        let sub = s.submit(snap("k")).unwrap();
        s.shutdown();
        drop(s);
        let mut journal: serde_json::Value = serde_json::from_slice(&fs::read(c.path().join("journal.json")).unwrap()).unwrap();
        assert!(journal.as_object_mut().unwrap().remove("access").is_some());
        let manifest = journal["pending"][0]["manifest"].as_object_mut().unwrap();
        assert!(manifest.remove("created_ms").is_some());
        let legacy = serde_json::to_vec(&journal).unwrap();
        assert!(!String::from_utf8_lossy(&legacy).contains("access") && !String::from_utf8_lossy(&legacy).contains("preview"));
        fs::write(c.path().join("journal.json"), legacy).unwrap();
        rewrite_manifest(c.path(), sub.version_id, |m| {
            m.remove("created_ms");
        });
        fs::remove_dir(c.path().join("previews")).unwrap();
        let mut s = offline(c.path());
        let state = s.poll();
        assert_eq!((state.pending, state.catalog.len(), state.catalog[0].created_ms), (1, 1, None));
        assert!(state.catalog[0].cached && state.catalog[0].pending && c.path().join("previews").is_dir());
        s.set_pinned(BTreeSet::from([sub.version_id]));
        s.set_cache_limit(u64::MAX);
        s.shutdown();
        let saved: Journal = serde_json::from_slice(&fs::read(c.path().join("journal.json")).unwrap()).unwrap();
        assert!(saved.access.contains_key(&sub.version_id) && saved.pending.len() == 1);
        assert!(offline(c.path()).poll().catalog[0].pending);
        let none: Journal = serde_json::from_slice(br#"{"bindings":{},"pending":[]}"#).unwrap();
        assert!(none.access.is_empty() && none.receipts.is_empty());
    }
    #[test]
    fn catalog_heads_are_newest_first_with_unknown_dates_last() {
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let old = s.submit(snap("old")).unwrap().version_id;
        let new = s.submit(snap("new")).unwrap().version_id;
        let undated = s.submit(snap("undated")).unwrap().version_id;
        let child_of = s.submit(snap("old")).unwrap().version_id;
        let sibling = {
            let first = lock(&s.shared).state.catalog.iter().find(|r| r.version_id == old).map(|r| r.project_id).unwrap();
            s.bind("fork", first, old).unwrap();
            s.submit(snap("fork")).unwrap().version_id
        };
        for (v, t) in [(old, Some(100)), (new, Some(300)), (undated, None), (child_of, Some(200)), (sibling, Some(150))] {
            lock(&s.shared).state.catalog.iter_mut().filter(|r| r.version_id == v).for_each(|r| r.created_ms = t);
        }
        let heads: Vec<_> = s.catalog_heads().iter().map(|r| r.version_id).collect();
        // `old` is a parent, so it is history; the forked project yields both of its heads.
        assert_eq!(heads, vec![new, child_of, sibling, undated]);
    }
    #[test]
    fn event_queue_overflow_keeps_fetch_results() {
        let c = tempfile::tempdir().unwrap();
        let s = offline(c.path());
        let v = s.submit(snap("k")).unwrap().version_id;
        let rev = s.poll().catalog.remove(0);
        let mut events: Vec<SyncEvent> = vec![SyncEvent::FetchFailed { version_id: v, error: "first".into() }];
        events.extend((0..400).map(|_| SyncEvent::Downloaded(rev.clone())));
        events.push(SyncEvent::Fetched { project_id: rev.project_id, version_id: v, path: rev.local_path.clone() });
        trim_events(&mut events);
        assert_eq!(events.len(), 256);
        assert!(matches!(events.first(), Some(SyncEvent::FetchFailed { .. })) && matches!(events.last(), Some(SyncEvent::Fetched { .. })));
        let mut only: Vec<SyncEvent> = (0..300).map(|n| SyncEvent::Error(n.to_string())).collect();
        trim_events(&mut only);
        assert!(only.len() == 256 && matches!(&only[0], SyncEvent::Error(m) if m == "44"));
    }
}
