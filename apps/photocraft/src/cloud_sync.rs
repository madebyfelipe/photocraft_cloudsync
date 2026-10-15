//! Desktop cloud shell. Snapshot capture is cheap; serialization, linked-file reads, durable
//! checkpoints, opening and the sync client all run on the worker, independently of Save/recovery.
use craft_cloud_sync::{Config, DownloadedRevision, Snapshot, SyncService, SyncStatus};
use photocraft_doc::{DocId, Document, Layer, LayerContent, SmartSource};
use photocraft_ui_egui::PhotocraftApp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use uuid::Uuid;

const DEBOUNCE: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct LocalCopy {
    key: String,
    title: String,
    path: PathBuf,
}
enum Command {
    Save { key: String, doc: Arc<Document>, revision: u64, token: String, source: Option<String> },
    Connect(Config),
    Open { path: PathBuf, key: String, binding: Option<(Uuid, Uuid)> },
    Refresh,
    Stop,
}
enum Reply {
    Ready { endpoint: String, username: String, password: String, connected: bool, credential_notice: String, bindings: HashMap<String, String> },
    Saved { key: String, revision: u64, source: Option<String>, checkpoint: bool, blocked: bool, result: Result<(), String> },
    State { status: SyncStatus, pending: usize, catalog: Vec<DownloadedRevision>, local: Vec<LocalCopy> },
    Opened { key: String, source: Option<String>, from_cloud: bool, result: Result<Box<Document>, String> },
    Connection { connected: bool, credential_notice: String },
    Error(String),
}

struct Tracked {
    key: String,
    observed: u64,
    durable: Option<u64>,
    queued: Option<u64>,
    blocked: bool,
    pending: Option<u64>,
    changed: Instant,
    first_change: Instant,
    doc: Arc<Document>,
    source: Option<String>,
}
impl Tracked {
    fn due(&self, now: Instant) -> bool {
        self.pending.is_none()
            && !self.blocked
            && self.queued != Some(self.observed)
            && (now.duration_since(self.changed) >= DEBOUNCE || now.duration_since(self.first_change) >= MAX_DELAY)
    }
    fn observe(&mut self, revision: u64, now: Instant) {
        if self.observed != revision {
            if self.durable == Some(self.observed) {
                self.first_change = now;
            }
            self.blocked = false;
            self.observed = revision;
            self.changed = now;
        }
    }
}

pub struct DesktopApp {
    inner: PhotocraftApp,
    tx: mpsc::Sender<Command>,
    rx: mpsc::Receiver<Reply>,
    tracked: HashMap<DocId, Tracked>,
    path_bindings: HashMap<String, String>,
    ready: bool,
    session_key: String,
    endpoint: String,
    username: String,
    password: String,
    credential_notice: String,
    window: bool,
    connected: bool,
    status: SyncStatus,
    pending: usize,
    catalog: Vec<DownloadedRevision>,
    local: Vec<LocalCopy>,
    error: Option<String>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl DesktopApp {
    pub fn new(inner: PhotocraftApp, automation: Option<photocraft_automation::AuthorizedWorkspace>) -> Self {
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let dir = crate::services::config_dir().map(|d| d.join("cloud-projects"));
        let spawn = std::thread::Builder::new().name("photocraft-cloud".into()).spawn(move || {
            if let Some(dir) = dir {
                worker(commands, replies, dir, automation);
            } else {
                let _ = replies.send(Reply::Error("No settings directory: cloud autosave is unavailable".into()));
            }
        });
        let (worker, error) = match spawn {
            Ok(w) => (Some(w), None),
            Err(e) => (None, Some(format!("Cannot start cloud autosave: {e}"))),
        };
        Self {
            inner,
            tx,
            rx,
            tracked: HashMap::new(),
            path_bindings: HashMap::new(),
            ready: false,
            session_key: Uuid::new_v4().to_string(),
            endpoint: String::new(),
            username: String::new(),
            password: String::new(),
            credential_notice: "Passwords use the system credential store when available.".into(),
            window: false,
            connected: false,
            status: SyncStatus::Idle,
            pending: 0,
            catalog: Vec::new(),
            local: Vec::new(),
            error,
            worker,
        }
    }
    fn tick(&mut self, ctx: &egui::Context) {
        loop {
            let reply = match self.rx.try_recv() {
                Ok(reply) => reply,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error.get_or_insert_with(|| "Cloud autosave worker stopped; use File › Save to preserve changes".into());
                    break;
                }
            };
            match reply {
                Reply::Ready { endpoint, username, password, connected, credential_notice, bindings } => {
                    self.ready = true;
                    self.path_bindings = bindings;
                    self.endpoint = endpoint;
                    self.username = username;
                    self.password = password;
                    self.connected = connected;
                    self.credential_notice = credential_notice;
                }
                Reply::Saved { key, revision, source, checkpoint, blocked, result } => {
                    for t in self.tracked.values_mut().filter(|t| t.key == key) {
                        if t.pending == Some(revision) {
                            t.pending = None;
                        }
                        if t.source == source {
                            if checkpoint {
                                t.durable = Some(revision);
                            }
                            if result.is_ok() {
                                t.queued = Some(revision);
                            } else {
                                t.blocked = blocked && t.observed == revision;
                                t.changed = Instant::now();
                                t.first_change = t.changed;
                            }
                        }
                    }
                    if let Err(e) = result {
                        self.error = Some(e);
                    } else if self.tracked.values().all(|t| !t.blocked && t.queued == Some(t.observed)) {
                        self.error = None;
                    }
                }
                Reply::State { status, pending, catalog, local } => {
                    if matches!(status, SyncStatus::Idle) && pending == 0 && self.tracked.values().all(|t| !t.blocked && t.queued == Some(t.observed)) {
                        self.error = None;
                    }
                    self.status = status;
                    self.pending = pending;
                    self.catalog = catalog;
                    self.local = local;
                }
                Reply::Opened { key, source, from_cloud, result } => match result {
                    Ok(doc) => {
                        if let Some(index) = self.inner.session.documents().iter().position(|d| self.tracked.get(&d.doc.id).is_some_and(|t| t.key == key)) {
                            self.inner.session.set_active(index);
                            continue;
                        }
                        let index = open_native_document(&mut self.inner, *doc);
                        if let Some(d) = self.inner.session.documents().get(index) {
                            let now = Instant::now();
                            self.tracked.insert(
                                d.doc.id,
                                Tracked {
                                    key,
                                    observed: d.revision,
                                    durable: Some(d.revision),
                                    queued: from_cloud.then_some(d.revision),
                                    blocked: false,
                                    pending: None,
                                    changed: now,
                                    first_change: now,
                                    doc: Arc::clone(&d.doc),
                                    source,
                                },
                            );
                        }
                    }
                    Err(e) => self.error = Some(e),
                },
                Reply::Connection { connected, credential_notice } => {
                    self.connected = connected;
                    self.credential_notice = credential_notice;
                }
                Reply::Error(e) => self.error = Some(e),
            }
        }
        if !self.ready {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }
        let now = Instant::now();
        for d in self.inner.session.documents() {
            let t = self.tracked.entry(d.doc.id).or_insert_with(|| Tracked {
                key: d
                    .path
                    .as_ref()
                    .map(|p| self.path_bindings.get(p).cloned().unwrap_or_else(|| path_key(p)))
                    .unwrap_or_else(|| format!("untitled-{}", Uuid::new_v4())),
                observed: d.revision,
                durable: None,
                queued: None,
                blocked: false,
                pending: None,
                changed: now,
                first_change: now,
                doc: Arc::clone(&d.doc),
                source: d.path.clone(),
            });
            t.observe(d.revision, now);
            t.doc = Arc::clone(&d.doc);
            if let Some(path) = &d.path {
                self.path_bindings.insert(path.clone(), t.key.clone());
            }
            if d.path.is_some() && t.source != d.path {
                // Save As can change the source path without changing the document revision.
                // Persist the same working key under its new path on the next checkpoint.
                t.source = d.path.clone();
                t.durable = None;
                t.queued = None;
                t.blocked = false;
                t.changed = now;
                t.first_change = now;
            }
            if t.due(now) {
                let command = Command::Save {
                    key: t.key.clone(),
                    doc: Arc::clone(&d.doc),
                    revision: d.revision,
                    token: format!("{}:{}", self.session_key, d.revision),
                    source: t.source.clone(),
                };
                if self.tx.send(command).is_ok() {
                    t.pending = Some(d.revision);
                    t.first_change = now;
                } else {
                    self.error = Some("Cloud autosave worker stopped; use Save to preserve changes".into());
                }
            }
        }
        let closed: Vec<_> = self.tracked.keys().filter(|id| !self.inner.session.documents().iter().any(|d| d.doc.id == **id)).copied().collect();
        for id in closed {
            if let Some(t) = self.tracked.remove(&id)
                && t.queued != Some(t.observed)
                && !t.blocked
                && t.pending != Some(t.observed)
            {
                let _ = self.tx.send(Command::Save {
                    key: t.key,
                    doc: t.doc,
                    revision: t.observed,
                    token: format!("{}:{}", self.session_key, t.observed),
                    source: t.source,
                });
            }
        }
        // Required even while typing continuously or while the window is otherwise idle.
        ctx.request_repaint_after(Duration::from_millis(250));
    }
    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Cloud Projects").clicked() {
                self.window = true;
                let _ = self.tx.send(Command::Refresh);
            }
            let unsettled = self.tracked.values().any(|t| t.durable != Some(t.observed));
            let label = if self.error.is_some() || matches!(self.status, SyncStatus::Error(_)) {
                "Error"
            } else if unsettled || matches!(self.status, SyncStatus::Syncing) {
                "Saving…"
            } else if !self.connected || matches!(self.status, SyncStatus::Offline) {
                "Offline · saved locally"
            } else if self.pending > 0 {
                "Saving…"
            } else {
                "Saved"
            };
            ui.small(label).on_hover_text("Cloud autosave is independent of File › Save and crash recovery.");
        });
        let mut open = self.window;
        egui::Window::new("Cloud Projects").open(&mut open).default_width(520.0).show(ui.ctx(), |ui| {
            ui.label("Connect your dedicated HTTPS WebDAV account.");
            ui.horizontal(|ui| {
                ui.label("Server");
                ui.text_edit_singleline(&mut self.endpoint);
            });
            ui.horizontal(|ui| {
                ui.label("Username");
                ui.text_edit_singleline(&mut self.username);
            });
            ui.horizontal(|ui| {
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
            });
            ui.small(&self.credential_notice);
            ui.small("Local autosaves remain available offline.");
            if ui.button("Connect / Refresh").clicked() {
                self.error = None;
                for t in self.tracked.values_mut() {
                    t.blocked = false;
                }
                let _ = self.tx.send(Command::Connect(Config {
                    endpoint: self.endpoint.clone(),
                    username: self.username.clone(),
                    password: self.password.clone(),
                }));
            }
            if let Some(e) = &self.error {
                ui.label(e);
            }
            if let SyncStatus::Error(e) = &self.status {
                ui.label(e);
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                ui.strong("Projects");
                for r in self.catalog.iter().filter(|r| r.heads.contains(&r.version_id)) {
                    revision_row(ui, &self.tx, r, false);
                }
                ui.collapsing("History", |ui| {
                    for r in self.catalog.iter().filter(|r| !r.heads.contains(&r.version_id)) {
                        revision_row(ui, &self.tx, r, true);
                    }
                });
                ui.separator();
                ui.strong("Local working copies");
                for copy in &self.local {
                    ui.horizontal(|ui| {
                        ui.label(&copy.title);
                        if ui.button("Open").clicked() {
                            let _ = self.tx.send(Command::Open { path: copy.path.clone(), key: copy.key.clone(), binding: None });
                        }
                    });
                }
            });
        });
        self.window = open;
    }
}
impl eframe::App for DesktopApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.inner.logic(ctx, frame);
        self.tick(ctx);
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.controls(ui);
        self.inner.ui(ui, frame);
    }
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        self.inner.raw_input_hook(ctx, input);
    }
    fn on_exit(&mut self) {
        for d in self.inner.session.documents() {
            let key = self
                .tracked
                .get(&d.doc.id)
                .map(|t| t.key.clone())
                .unwrap_or_else(|| d.path.as_ref().map(|p| path_key(p)).unwrap_or_else(|| format!("untitled-{}", Uuid::new_v4())));
            if self.tracked.get(&d.doc.id).is_none_or(|t| t.queued != Some(d.revision) && !t.blocked && t.pending != Some(d.revision)) {
                let _ = self.tx.send(Command::Save {
                    key,
                    doc: Arc::clone(&d.doc),
                    revision: d.revision,
                    token: format!("{}:{}", self.session_key, d.revision),
                    source: d.path.clone(),
                });
            }
        }
        let _ = self.tx.send(Command::Stop);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("Cloud autosave worker failed during shutdown");
        }
        self.inner.on_exit();
    }
}

/// Background completion can arrive after the shell's per-frame view synchronization.
/// Install the document and its UI view together before chrome/canvas rendering begins.
fn open_native_document(app: &mut PhotocraftApp, document: Document) -> usize {
    let index = app.session.add_document(document, None);
    app.sync_views();
    index
}

fn revision_row(ui: &mut egui::Ui, tx: &mpsc::Sender<Command>, revision: &DownloadedRevision, history: bool) {
    ui.horizontal(|ui| {
        let suffix = if history {
            format!(" · revision {}", revision.version_id.to_string().chars().take(8).collect::<String>())
        } else if revision.heads.len() > 1 {
            format!(" · conflict copy {}/{}", revision.heads.iter().position(|v| *v == revision.version_id).map(|i| i + 1).unwrap_or(1), revision.heads.len())
        } else {
            String::new()
        };
        ui.label(format!("{}{suffix}", revision.title)).on_hover_text(format!("Revision {}", revision.version_id));
        if ui.button("Open").clicked() {
            let _ = tx.send(Command::Open {
                path: revision.local_path.clone(),
                key: format!("cloud-{}", Uuid::new_v4()),
                binding: Some((revision.project_id, revision.version_id)),
            });
        }
    });
}
fn load_native_project(path: &Path) -> Result<Document, String> {
    photocraft_format::load_path_with(path, &photocraft_format::LoadOptions { preserve_ids: false, ..Default::default() }).map_err(|e| e.to_string())
}

fn load_native_project_with_access(path: &Path, automation: Option<&photocraft_automation::AuthorizedWorkspace>) -> Result<Document, String> {
    if let Some(workspace) = automation {
        let path = path.to_str().ok_or("Cloud cache path is not valid UTF-8")?;
        let bytes = workspace.read(path).map_err(|e| format!("Cloud Open in control mode requires the cache inside --automation-read-root: {e}"))?;
        photocraft_format::load_from_bytes_with(&bytes, &photocraft_format::LoadOptions { preserve_ids: false, ..Default::default() })
            .map_err(|e| e.to_string())
    } else {
        load_native_project(path)
    }
}

fn path_key(path: &str) -> String {
    format!("file-{:x}", Sha256::digest(path.as_bytes()))
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Capture original linked bytes, never the flattened render cache. The editor's document
/// remains untouched. Missing originals stop publication, while the local native checkpoint survives.
#[cfg(test)]
fn portable(doc: &Document, source: Option<&str>) -> Result<Document, String> {
    portable_with_access(doc, source, None)
}
fn portable_with_access(doc: &Document, source: Option<&str>, automation: Option<&photocraft_automation::AuthorizedWorkspace>) -> Result<Document, String> {
    let root = source.and_then(|s| Path::new(s).parent()).and_then(|p| std::fs::canonicalize(if p.as_os_str().is_empty() { Path::new(".") } else { p }).ok());
    portable_document(doc, source, 0, root.as_deref(), automation).map(|(doc, _)| doc)
}
fn portable_document(
    doc: &Document,
    source: Option<&str>,
    source_depth: usize,
    approved_root: Option<&Path>,
    automation: Option<&photocraft_automation::AuthorizedWorkspace>,
) -> Result<(Document, bool), String> {
    if source_depth > 16 {
        return Err("Saved locally; nested smart sources exceed the portability limit".into());
    }
    let mut copy = doc.clone();
    let mut changed = false;
    let mut todo: Vec<(&mut Layer, usize)> = copy.layers.iter_mut().map(|l| (l, 0)).collect();
    while let Some((layer, depth)) = todo.pop() {
        let mut linked_origin = None;
        if depth > 256 {
            return Err("Linked assets: layer nesting exceeds the portability limit".into());
        }
        if let LayerContent::Smart(smart) = &mut layer.content
            && let SmartSource::Linked { path } = &smart.source
        {
            let (file_name, bytes) = if let Some(f) = photocraft_io::linked::find_linked_file(&doc.metadata, path) {
                (f.file_name, f.bytes)
            } else {
                let root = approved_root.ok_or_else(|| {
                    "Saved locally; linked originals require a saved project directory. Embed the original using Place Embedded before syncing.".to_owned()
                })?;
                let linked = Path::new(path);
                let resolved = if linked.is_relative() {
                    source.and_then(|s| Path::new(s).parent()).map(|p| p.join(linked)).unwrap_or_else(|| linked.to_owned())
                } else {
                    linked.to_owned()
                };
                let resolved = std::fs::canonicalize(&resolved).map_err(|e| format!("Saved locally; cannot resolve linked original {path}: {e}"))?;
                if !resolved.starts_with(root) {
                    return Err(format!("Saved locally; linked original {path} is outside the project directory. Use Place Embedded before syncing."));
                }
                let file = resolved.as_path();
                linked_origin = Some(resolved.clone());
                let bytes = if let Some(workspace) = automation {
                    workspace
                        .read(file.to_str().ok_or("Saved locally; linked original path is not UTF-8")?)
                        .map_err(|e| format!("Saved locally; linked original is outside the control read grant: {e}"))?
                } else {
                    use std::io::Read;
                    let original = std::fs::File::open(file).map_err(|e| format!("Saved locally; cannot sync missing linked original {path}: {e}"))?;
                    let mut bytes = Vec::new();
                    original
                        .take(256 * 1024 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| format!("Saved locally; cannot read linked original {path}: {e}"))?;
                    bytes
                };
                if bytes.len() > 256 * 1024 * 1024 {
                    return Err(format!("Saved locally; linked original {path} exceeds the cloud payload limit"));
                }
                let file_name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "linked-original".into());
                if photocraft_format::is_pcraft(&bytes) || photocraft_io::is_psd(&bytes) {
                    photocraft_io::import(&file_name, &bytes)
                        .map_err(|e| format!("Saved locally; linked original {path} is not a readable native/image source: {e}"))?;
                } else {
                    photocraft_codecs::decode(&bytes).map_err(|_| {
                        format!("Saved locally; linked original {path} is not a supported self-contained image. Use Place Embedded before syncing.")
                    })?;
                }
                (file_name, bytes)
            };
            smart.source = SmartSource::Embedded { file_name, bytes: Arc::new(bytes) };
            changed = true;
        }
        if let LayerContent::Smart(smart) = &mut layer.content
            && let SmartSource::Embedded { file_name, bytes } = &mut smart.source
            && photocraft_format::is_pcraft(bytes)
        {
            let nested =
                photocraft_format::load_from_bytes(bytes).map_err(|e| format!("Saved locally; cannot inspect nested native source {file_name}: {e}"))?;
            let base = linked_origin.unwrap_or_else(|| {
                source.and_then(|s| Path::new(s).parent()).map(|p| p.join(file_name.as_str())).unwrap_or_else(|| PathBuf::from(file_name.as_str()))
            });
            let (nested, nested_changed) = portable_document(&nested, base.to_str(), source_depth + 1, approved_root, automation)?;
            if nested_changed {
                *bytes = Arc::new(photocraft_format::save_to_bytes(&nested, &Default::default()).map_err(|e| e.to_string())?);
                changed = true;
            }
        }
        if let LayerContent::Smart(smart) = &layer.content
            && let SmartSource::Embedded { file_name, bytes } = &smart.source
            && photocraft_io::is_psd(bytes)
        {
            validate_psd_dependencies(file_name, bytes, source_depth + 1)?;
        }
        if let Some(children) = layer.children_mut() {
            todo.extend(children.iter_mut().map(|l| (l, depth + 1)));
        }
    }
    Ok((copy, changed))
}

/// PSD bytes remain original. Inspect through the existing importer and reject external
/// dependencies that cannot be made self-contained without rewriting the placed PSD.
fn validate_psd_dependencies(name: &str, bytes: &[u8], depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("Saved locally; nested PSD smart sources exceed the portability limit".into());
    }
    let imported = photocraft_io::import(name, bytes).map_err(|e| format!("Saved locally; cannot inspect PSD smart source {name}: {e}"))?;
    let doc = imported.document;
    let mut todo: Vec<(&Layer, usize)> = doc.layers.iter().map(|l| (l, 0)).collect();
    while let Some((layer, layer_depth)) = todo.pop() {
        if layer_depth > 256 {
            return Err("Saved locally; PSD layer nesting exceeds the portability limit".into());
        }
        if let LayerContent::Smart(smart) = &layer.content {
            let original = match &smart.source {
                SmartSource::Embedded { file_name, bytes } => Some((file_name.clone(), Arc::clone(bytes))),
                SmartSource::Linked { path } => photocraft_io::linked::find_linked_file(&doc.metadata, path).map(|f| (f.file_name, Arc::new(f.bytes))),
            }
            .ok_or_else(|| {
                format!("Saved locally; PSD smart source {name} has an external linked dependency. Embed its original in the source PSD before syncing.")
            })?;
            if photocraft_format::is_pcraft(&original.1) {
                let nested =
                    photocraft_format::load_from_bytes(&original.1).map_err(|e| format!("Saved locally; cannot inspect native source inside PSD: {e}"))?;
                let (_, changed) = portable_document(&nested, None, depth + 1, None, None)?;
                if changed {
                    return Err(format!(
                        "Saved locally; native source inside PSD {name} has linked dependencies. Embed them in the source PSD before syncing."
                    ));
                }
            } else if photocraft_io::is_psd(&original.1) {
                validate_psd_dependencies(&original.0, &original.1, depth + 1)?;
            }
        }
        if let Some(children) = layer.children() {
            todo.extend(children.iter().map(|l| (l, layer_depth + 1)));
        }
    }
    Ok(())
}

fn local_copies(dir: &Path) -> Vec<LocalCopy> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut copies: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                return None;
            }
            let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
            let key = value.get("key")?.as_str()?.to_owned();
            let payload = value.get("payload").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("{key}.pcraft"));
            Some(LocalCopy { title: value.get("title")?.as_str()?.to_owned(), path: dir.join(payload), key })
        })
        .collect();
    copies.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.key.cmp(&b.key)));
    copies
}
fn retry_pending(dir: &Path, service: &SyncService) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut errors = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let result = (|| {
            let mut metadata: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            if metadata["needs_submit"] != true || metadata["portable"] != true {
                return Ok(());
            }
            let key = metadata["key"].as_str().ok_or("invalid pending working key")?.to_owned();
            let title = metadata["title"].as_str().ok_or("invalid pending working title")?.to_owned();
            let token = metadata["local_token"].as_str().ok_or("invalid pending working token")?.to_owned();
            let payload = metadata["payload"].as_str().ok_or("invalid pending working payload")?;
            if Path::new(payload).file_name().and_then(|n| n.to_str()) != Some(payload) {
                return Err("invalid pending working payload path".into());
            }
            use std::io::Read;
            let mut native = Vec::new();
            std::fs::File::open(dir.join(payload))
                .map_err(|e| e.to_string())?
                .take(256 * 1024 * 1024 + 1)
                .read_to_end(&mut native)
                .map_err(|e| e.to_string())?;
            if native.len() > 256 * 1024 * 1024 {
                return Err("Saved locally; native checkpoint exceeds cloud payload limit".into());
            }
            let hash = format!("{:x}", Sha256::digest(&native));
            if metadata["sha256"].as_str() != Some(hash.as_str()) {
                return Err("Saved locally; pending checkpoint checksum differs; preserved for recovery".into());
            }
            service.submit(Snapshot { local_key: key, title, extension: "pcraft".into(), local_token: token, native }).map_err(|e| e.to_string())?;
            metadata["needs_submit"] = json!(false);
            atomic_write(&path, &serde_json::to_vec(&metadata).map_err(|e| e.to_string())?)?;
            Ok::<(), String>(())
        })();
        if let Err(error) = result {
            errors.push(error);
        }
    }
    errors
}

fn worker(commands: mpsc::Receiver<Command>, replies: mpsc::Sender<Reply>, dir: PathBuf, automation: Option<photocraft_automation::AuthorizedWorkspace>) {
    if let Err(e) = std::fs::create_dir_all(dir.join("working")) {
        let _ = replies.send(Reply::Error(e.to_string()));
        return;
    }
    let settings = dir.join("account.json");
    let account: Value = std::fs::read(&settings).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let mut bindings = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("working")) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().is_some_and(|e| e == "json")
                && let Ok(bytes) = std::fs::read(entry.path())
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && let (Some(source), Some(key)) = (value["source"].as_str(), value["key"].as_str())
            {
                bindings.insert(source.to_owned(), key.to_owned());
            }
            if let Ok(bytes) = std::fs::read(entry.path())
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && let Some(error) = value["portability_error"].as_str()
            {
                let _ = replies.send(Reply::Error(error.to_owned()));
            }
        }
    }
    let endpoint = account["endpoint"].as_str().unwrap_or_default().to_owned();
    let username = account["username"].as_str().unwrap_or_default().to_owned();
    let mut credential_notice = "Passwords use the system credential store when available; otherwise they stay in memory for this session.".to_owned();
    let restored_password = if endpoint.is_empty() {
        None
    } else {
        match craft_cloud_sync::credentials::load_credential("photocraft", &endpoint, &username) {
            Ok(Some(password)) => {
                credential_notice = "Password saved in the system credential store. Sync resumes automatically.".into();
                Some(password)
            }
            Ok(None) => {
                credential_notice = "Enter your password to connect. It will be saved in the system credential store when available.".into();
                None
            }
            Err(_) => {
                credential_notice = "Secure credential store unavailable. Password stays in memory for this session.".into();
                None
            }
        }
    };
    let connected = restored_password.is_some();
    let password = restored_password.unwrap_or_default();
    let config = if connected {
        Config { endpoint: endpoint.clone(), username: username.clone(), password: password.clone() }
    } else {
        Config { endpoint: String::new(), username: String::new(), password: String::new() }
    };
    let _ = replies.send(Reply::Ready { bindings, endpoint, username, password, connected, credential_notice });
    let mut service = match SyncService::new(config, dir.join("sync"), "photocraft") {
        Ok(s) => Some(s),
        Err(e) => {
            let _ = replies.send(Reply::Error(format!("Sync queue unavailable; local checkpoints continue: {e}")));
            None
        }
    };
    let mut next_retry = Instant::now();
    loop {
        if Instant::now() >= next_retry {
            if let Some(service) = &service {
                for error in retry_pending(&dir.join("working"), service) {
                    let _ = replies.send(Reply::Error(error));
                }
            }
            next_retry = Instant::now() + Duration::from_secs(2);
        }
        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(Command::Save { key, doc, revision, token, source }) => {
                let mut durable = false;
                let mut blocked = false;
                let result = crate::crash_guard::guard("Cloud autosave", || {
                    let portable = portable_with_access(&doc, source.as_deref(), automation.as_ref());
                    let checkpoint_doc = portable.as_ref().unwrap_or(&doc);
                    let native = photocraft_format::save_to_bytes(checkpoint_doc, &Default::default()).map_err(|e| e.to_string())?;
                    let hash = format!("{:x}", Sha256::digest(&native));
                    let local_token = format!("{token}:{hash}");
                    let payload = format!("{key}-{hash}.pcraft");
                    atomic_write(&dir.join("working").join(&payload), &native)?;
                    let mut metadata = json!({"key":key,"title":doc.name,"source":source.clone(),"payload":payload,"sha256":hash,"local_token":local_token,"portable":portable.is_ok(),"needs_submit":portable.is_ok(),"portability_error":portable.as_ref().err()});
                    let metadata_path = dir.join("working").join(format!("{key}.json"));
                    atomic_write(&metadata_path, &serde_json::to_vec(&metadata).map_err(|e| e.to_string())?)?;
                    durable = true;
                    if let Err(error) = portable {
                        blocked = true;
                        return Err(error);
                    }
                    service
                        .as_ref()
                        .ok_or("Saved locally; sync queue unavailable")?
                        .submit(Snapshot { local_key: key.clone(), title: doc.name.clone(), extension: "pcraft".into(), local_token, native })
                        .map_err(|e| e.to_string())?;
                    metadata["needs_submit"] = json!(false);
                    atomic_write(&metadata_path, &serde_json::to_vec(&metadata).map_err(|e| e.to_string())?)?;
                    Ok(())
                });
                let _ = replies.send(Reply::Saved { key, revision, source, checkpoint: durable, blocked, result });
            }
            Ok(Command::Connect(config)) => {
                let credentials = config.clone();
                let public = json!({"endpoint":config.endpoint,"username":config.username});
                if let Some(service) = &mut service {
                    service.shutdown();
                }
                match SyncService::new(config, dir.join("sync"), "photocraft") {
                    Ok(s) => {
                        service = Some(s);
                        let connected = !credentials.endpoint.is_empty();
                        let credential_notice = if !connected {
                            "Local autosave is active. Connect a WebDAV account to sync.".into()
                        } else {
                            match craft_cloud_sync::credentials::save_credential(
                                "photocraft",
                                &credentials.endpoint,
                                &credentials.username,
                                &credentials.password,
                            ) {
                                Ok(()) => "Password saved in the system credential store. Sync resumes automatically.".into(),
                                Err(_) => "Secure credential store unavailable. Password stays in memory for this session.".into(),
                            }
                        };
                        let _ = replies.send(Reply::Connection { connected, credential_notice });
                        if let Ok(bytes) = serde_json::to_vec(&public)
                            && let Err(e) = atomic_write(&settings, &bytes)
                        {
                            let _ = replies.send(Reply::Error(e));
                        }
                    }
                    Err(e) => {
                        let _ = replies.send(Reply::Error(e.to_string()));
                        let _ = replies
                            .send(Reply::Connection { connected: false, credential_notice: "Local autosave is active. Check the connection settings.".into() });
                        match SyncService::new(
                            Config { endpoint: String::new(), username: String::new(), password: String::new() },
                            dir.join("sync"),
                            "photocraft",
                        ) {
                            Ok(local) => service = Some(local),
                            Err(local_error) => {
                                service = None;
                                let _ = replies
                                    .send(Reply::Error(format!("Cloud connection failed; local checkpoints continue, sync queue unavailable: {local_error}")));
                            }
                        }
                    }
                }
            }
            Ok(Command::Open { path, key, binding }) => {
                let result = crate::crash_guard::guard("Open cloud project", || {
                    let document = load_native_project_with_access(&path, automation.as_ref())?;
                    if let Some((project, version)) = binding {
                        service
                            .as_ref()
                            .ok_or("Sync queue unavailable; try opening the local working copy")?
                            .bind(&key, project, version)
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(document)
                });
                let source = std::fs::read(dir.join("working").join(format!("{key}.json")))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| v.get("source").and_then(Value::as_str).map(str::to_owned));
                let _ = replies.send(Reply::Opened { key, source, from_cloud: binding.is_some(), result: result.map(Box::new) });
            }
            Ok(Command::Refresh) => {
                if let Some(service) = &service {
                    service.request_sync();
                }
            }
            Ok(Command::Stop) => {
                if let Some(service) = &mut service {
                    service.shutdown();
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(service) = &mut service {
                    service.shutdown();
                }
                break;
            }
        }
        let state = service.as_ref().map(SyncService::poll).unwrap_or_else(|| craft_cloud_sync::SyncState {
            status: SyncStatus::Error("Sync queue unavailable; local checkpoints continue".into()),
            ..Default::default()
        });
        for event in state.events {
            if let craft_cloud_sync::SyncEvent::Error(message) = event {
                let _ = replies.send(Reply::Error(message));
            }
        }
        let _ = replies.send(Reply::State { status: state.status, pending: state.pending, catalog: state.catalog, local: local_copies(&dir.join("working")) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::{Affine, SmartObject};
    fn doc() -> Document {
        Document::new("Untitled", photocraft_geom::Size::new(2, 2), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8)
    }
    #[test]
    fn continuous_edits_hit_maximum_interval_and_completion_keeps_newer_work() {
        let start = Instant::now();
        let mut t = Tracked {
            key: "x".into(),
            observed: 1,
            durable: None,
            queued: None,
            blocked: false,
            pending: None,
            changed: start,
            first_change: start,
            doc: Arc::new(doc()),
            source: None,
        };
        for revision in 2..18 {
            t.observe(revision, start + Duration::from_secs(revision - 1));
        }
        assert!(t.due(start + Duration::from_secs(17)));
        t.pending = Some(17);
        t.observe(18, start + Duration::from_secs(18));
        t.pending = None;
        t.durable = Some(17);
        t.queued = Some(17);
        assert!(t.due(start + Duration::from_secs(20)));
        // A durable checkpoint must not suppress retry when queue submission failed.
        t.durable = Some(18);
        t.queued = None;
        assert!(t.due(start + Duration::from_secs(20)));
        t.blocked = true;
        assert!(!t.due(start + Duration::from_secs(20)));
        t.observe(19, start + Duration::from_secs(21));
        assert!(t.due(start + Duration::from_secs(23)));
    }
    #[test]
    fn linked_original_is_embedded_without_changing_editor_document() {
        let path = std::env::temp_dir().join(format!("photocraft-cloud-{}", Uuid::new_v4()));
        let original_bytes = photocraft_format::save_to_bytes(&doc(), &Default::default()).unwrap();
        std::fs::write(&path, &original_bytes).unwrap();
        let source = path.with_extension("project.pcraft");
        let mut d = doc();
        d.layers.push(Layer::new(
            "linked",
            LayerContent::Smart(SmartObject::new(SmartSource::Linked { path: path.to_string_lossy().into_owned() }, Affine::IDENTITY, None)),
        ));
        let p = portable(&d, source.to_str()).unwrap();
        assert!(matches!(&d.layers[0].content, LayerContent::Smart(s) if matches!(s.source, SmartSource::Linked { .. })));
        assert!(
            matches!(&p.layers[0].content, LayerContent::Smart(s) if matches!(&s.source, SmartSource::Embedded { bytes, .. } if bytes.as_slice() == original_bytes.as_slice()))
        );
        let mut container = doc();
        container.layers.push(Layer::new(
            "nested native",
            LayerContent::Smart(SmartObject::new(
                SmartSource::Embedded { file_name: "inner.pcraft".into(), bytes: Arc::new(photocraft_format::save_to_bytes(&d, &Default::default()).unwrap()) },
                Affine::IDENTITY,
                None,
            )),
        ));
        let portable_container = portable(&container, source.to_str()).unwrap();
        let LayerContent::Smart(smart) = &portable_container.layers[0].content else { panic!("missing nested source") };
        let SmartSource::Embedded { bytes, .. } = &smart.source else { panic!("missing embedded source") };
        let nested = photocraft_format::load_from_bytes(bytes).unwrap();
        assert!(
            matches!(&nested.layers[0].content, LayerContent::Smart(s) if matches!(&s.source, SmartSource::Embedded { bytes, .. } if bytes.as_slice() == original_bytes.as_slice()))
        );
        std::fs::remove_file(path).unwrap();
        assert!(portable(&d, source.to_str()).unwrap_err().contains("Saved locally"));
        assert!(portable(&container, source.to_str()).unwrap_err().contains("Saved locally"));
    }
    #[test]
    fn linked_native_sources_resolve_dependencies_from_their_own_directory() {
        let dir = std::env::temp_dir().join(format!("photocraft-cloud-relative-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        let original_bytes = photocraft_format::save_to_bytes(&doc(), &Default::default()).unwrap();
        std::fs::write(dir.join("assets/original.bin"), &original_bytes).unwrap();
        let mut inner = doc();
        inner
            .layers
            .push(Layer::new("linked", LayerContent::Smart(SmartObject::new(SmartSource::Linked { path: "original.bin".into() }, Affine::IDENTITY, None))));
        atomic_write(&dir.join("assets/inner.pcraft"), &photocraft_format::save_to_bytes(&inner, &Default::default()).unwrap()).unwrap();
        let mut outer = doc();
        outer.layers.push(Layer::new(
            "native source",
            LayerContent::Smart(SmartObject::new(SmartSource::Linked { path: "assets/inner.pcraft".into() }, Affine::IDENTITY, None)),
        ));
        let portable = portable(&outer, dir.join("outer.pcraft").to_str()).unwrap();
        let LayerContent::Smart(smart) = &portable.layers[0].content else { panic!("missing smart source") };
        let SmartSource::Embedded { bytes, .. } = &smart.source else { panic!("missing embedded native source") };
        let nested = photocraft_format::load_from_bytes(bytes).unwrap();
        assert!(
            matches!(&nested.layers[0].content, LayerContent::Smart(s) if matches!(&s.source, SmartSource::Embedded { bytes, .. } if bytes.as_slice() == original_bytes.as_slice()))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn placed_psd_external_dependencies_are_reported_before_publication() {
        let mut source = doc();
        source.layers.push(Layer::new(
            "placed",
            LayerContent::Smart(SmartObject::new(
                SmartSource::Embedded { file_name: "original.bin".into(), bytes: Arc::new(b"original".to_vec()) },
                Affine::IDENTITY,
                None,
            )),
        ));
        let mut psd = photocraft_io::export(&source, "psd", &Default::default()).unwrap().bytes;
        validate_psd_dependencies("self-contained.psd", &psd, 0).unwrap();
        let offset = psd.windows(4).position(|bytes| bytes == b"liFD").unwrap();
        psd[offset..offset + 4].copy_from_slice(b"liFE");
        assert!(validate_psd_dependencies("external.psd", &psd, 0).unwrap_err().contains("external linked dependency"));
    }
    #[test]
    fn opening_cached_project_initializes_views_before_the_next_shell_frame() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        assert!(app.ui.views.is_empty());
        let document = doc();
        let first = open_native_document(&mut app, document.clone());
        assert!(app.ui.views.get(first).is_some());
        assert_eq!(app.session.active_index(), Some(first));
        let second = open_native_document(&mut app, document);
        assert!(app.ui.views.get(second).is_some());
        assert_eq!(app.ui.views.len(), app.session.documents().len());
        assert_eq!(app.session.active_index(), Some(second));
    }
    #[test]
    fn opening_native_revisions_allocates_distinct_runtime_document_and_layer_ids() {
        let path = std::env::temp_dir().join(format!("photocraft-cloud-open-{}.pcraft", Uuid::new_v4()));
        let mut original = doc();
        original.layers.push(Layer::new(
            "asset",
            LayerContent::Smart(SmartObject::new(
                SmartSource::Embedded { file_name: "asset.bin".into(), bytes: Arc::new(b"asset".to_vec()) },
                Affine::IDENTITY,
                None,
            )),
        ));
        atomic_write(&path, &photocraft_format::save_to_bytes(&original, &Default::default()).unwrap()).unwrap();
        let first = load_native_project(&path).unwrap();
        let second = load_native_project(&path).unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(first.id, original.id);
        assert_ne!(first.layers[0].id, second.layers[0].id);
        assert_ne!(first.layers[0].id, original.layers[0].id);
        let denied = photocraft_automation::AuthorizedWorkspace::new(None, None).unwrap();
        assert!(load_native_project_with_access(&path, Some(&denied)).unwrap_err().contains("automation-read-root"));
        let allowed = photocraft_automation::AuthorizedWorkspace::new(path.parent(), None).unwrap();
        assert!(load_native_project_with_access(&path, Some(&allowed)).is_ok());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn outside_project_and_symlinked_or_opaque_assets_remain_local() {
        let dir = std::env::temp_dir().join(format!("photocraft-cloud-scope-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::create_dir_all(dir.join("private")).unwrap();
        let outside = dir.join("private/original.pcraft");
        atomic_write(&outside, &photocraft_format::save_to_bytes(&doc(), &Default::default()).unwrap()).unwrap();
        let source = dir.join("project/project.pcraft").to_string_lossy().into_owned();
        let linked_doc = |path: &Path| {
            let mut d = doc();
            d.layers.push(Layer::new(
                "linked",
                LayerContent::Smart(SmartObject::new(SmartSource::Linked { path: path.to_string_lossy().into_owned() }, Affine::IDENTITY, None)),
            ));
            d
        };
        let malicious = linked_doc(&outside);
        assert!(portable(&malicious, Some(&source)).unwrap_err().contains("outside the project directory"));
        assert!(portable(&malicious, None).unwrap_err().contains("saved project directory"));
        #[cfg(unix)]
        {
            let symlink = dir.join("project/alias.pcraft");
            std::os::unix::fs::symlink(&outside, &symlink).unwrap();
            assert!(portable(&linked_doc(&symlink), Some(&source)).unwrap_err().contains("outside the project directory"));
        }
        let inside = dir.join("project/original.pcraft");
        std::fs::copy(&outside, &inside).unwrap();
        let denied = photocraft_automation::AuthorizedWorkspace::new(None, None).unwrap();
        assert!(portable_with_access(&linked_doc(&inside), Some(&source), Some(&denied)).unwrap_err().contains("control read grant"));
        let allowed = photocraft_automation::AuthorizedWorkspace::new(Some(&dir.join("project")), None).unwrap();
        assert!(portable_with_access(&linked_doc(&inside), Some(&source), Some(&allowed)).is_ok());
        let opaque = dir.join("project/credential.txt");
        std::fs::write(&opaque, b"private credential data").unwrap();
        assert!(portable(&linked_doc(&opaque), Some(&source)).unwrap_err().contains("supported self-contained image"));
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let cache = dir.join("cache");
        let worker_cache = cache.clone();
        let handle = std::thread::spawn(move || worker(commands, replies, worker_cache, None));
        tx.send(Command::Save { key: "malicious".into(), doc: Arc::new(malicious), revision: 1, token: "scope-test".into(), source: Some(source) }).unwrap();
        loop {
            if let Reply::Saved { checkpoint, blocked, result, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                assert!(checkpoint);
                assert!(blocked);
                assert!(result.is_err());
                break;
            }
        }
        tx.send(Command::Stop).unwrap();
        handle.join().unwrap();
        let copies = local_copies(&cache.join("working"));
        assert_eq!(copies.len(), 1);
        let checkpoint = photocraft_format::load_path(&copies[0].path).unwrap();
        assert!(matches!(&checkpoint.layers[0].content, LayerContent::Smart(s) if matches!(&s.source, SmartSource::Linked { .. })));
        let mut service =
            SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, cache.join("sync"), "photocraft").unwrap();
        assert!(retry_pending(&cache.join("working"), &service).is_empty());
        assert_eq!(service.poll().pending, 0);
        assert!(service.poll().catalog.is_empty());
        service.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unavailable_shared_queue_keeps_local_checkpoint_and_restart_enqueues_it() {
        let dir = std::env::temp_dir().join(format!("photocraft-cloud-queue-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sync")).unwrap();
        std::fs::write(dir.join("sync/journal.json"), b"corrupt journal").unwrap();
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let worker_dir = dir.clone();
        let handle = std::thread::spawn(move || worker(commands, replies, worker_dir, None));
        tx.send(Command::Save { key: "working-only".into(), doc: Arc::new(doc()), revision: 1, token: "durable-intent".into(), source: None }).unwrap();
        loop {
            if let Reply::Saved { checkpoint, blocked, result, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                assert!(checkpoint);
                assert!(!blocked);
                assert!(result.unwrap_err().contains("Saved locally"));
                break;
            }
        }
        tx.send(Command::Stop).unwrap();
        handle.join().unwrap();
        assert_eq!(local_copies(&dir.join("working")).len(), 1);
        std::fs::remove_file(dir.join("sync/journal.json")).unwrap();
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let worker_dir = dir.clone();
        let handle = std::thread::spawn(move || worker(commands, replies, worker_dir, None));
        loop {
            if let Reply::State { pending, catalog, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                assert_eq!(pending, 1);
                assert_eq!(catalog.len(), 1);
                break;
            }
        }
        tx.send(Command::Stop).unwrap();
        handle.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn background_native_checkpoint_survives_restart_without_persisting_password() {
        let dir = std::env::temp_dir().join(format!("photocraft-cloud-worker-{}", Uuid::new_v4()));
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let worker_dir = dir.clone();
        let handle = std::thread::spawn(move || worker(commands, replies, worker_dir, None));
        tx.send(Command::Connect(Config { endpoint: String::new(), username: "account".into(), password: "never-persist-this".into() })).unwrap();
        let d = Arc::new(doc());
        tx.send(Command::Save { key: "untitled-test".into(), doc: Arc::clone(&d), revision: 3, token: "session:3".into(), source: None }).unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Reply::Saved { revision, checkpoint, blocked, result, .. } => {
                    assert_eq!(revision, 3);
                    assert!(checkpoint);
                    assert!(!blocked);
                    result.unwrap();
                    break;
                }
                Reply::Error(e) => panic!("worker failed: {e}"),
                _ => {}
            }
        }
        let mut missing = doc();
        missing.layers.push(Layer::new(
            "missing",
            LayerContent::Smart(SmartObject::new(SmartSource::Linked { path: dir.join("missing.psd").to_string_lossy().into_owned() }, Affine::IDENTITY, None)),
        ));
        tx.send(Command::Save { key: "missing-test".into(), doc: Arc::new(missing), revision: 4, token: "session:4".into(), source: None }).unwrap();
        loop {
            if let Reply::Saved { checkpoint, blocked, result, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                assert!(checkpoint);
                assert!(blocked);
                assert!(result.unwrap_err().contains("Saved locally"));
                break;
            }
        }
        tx.send(Command::Stop).unwrap();
        handle.join().unwrap();
        let copies = local_copies(&dir.join("working"));
        assert_eq!(copies.len(), 2);
        let copy = copies.iter().find(|c| c.key == "untitled-test").unwrap();
        assert_eq!(photocraft_format::load_path(&copy.path).unwrap().name, d.name);
        let saved_account = std::fs::read_to_string(dir.join("account.json")).unwrap();
        assert!(!saved_account.contains("never-persist-this"));
        let mut restarted =
            SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, dir.join("sync"), "photocraft").unwrap();
        assert_eq!(restarted.poll().pending, 1);
        assert_eq!(restarted.poll().catalog.len(), 1);
        let metadata_path = dir.join("working/untitled-test.json");
        let mut metadata: Value = serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
        // Simulate termination after shared submit, before clearing the durable working intent.
        metadata["needs_submit"] = json!(true);
        atomic_write(&metadata_path, &serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(retry_pending(&dir.join("working"), &restarted).is_empty());
        assert_eq!(restarted.poll().pending, 1);
        // Simulate termination with only the working checkpoint durable, before shared submit.
        metadata["key"] = json!("working-only");
        metadata["local_token"] = json!("persisted-before-crash");
        metadata["needs_submit"] = json!(true);
        atomic_write(&dir.join("working/working-only.json"), &serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(retry_pending(&dir.join("working"), &restarted).is_empty());
        assert_eq!(restarted.poll().pending, 2);
        assert_eq!(restarted.poll().catalog.len(), 2);
        // The missing-original checkpoint remains blocked and is never submitted on restart.
        let blocked: Value = serde_json::from_slice(&std::fs::read(dir.join("working/missing-test.json")).unwrap()).unwrap();
        assert_eq!(blocked["portable"], false);
        assert_eq!(blocked["needs_submit"], false);
        restarted.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
