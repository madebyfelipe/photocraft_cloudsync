//! Desktop cloud shell. Snapshot capture is cheap; serialization, linked-file reads, durable
//! checkpoints, previews, opening and the sync client all run on the worker, independently of
//! Save/recovery. The project library (Home) lives in `cloud_home` / `cloud_home_ui`; this file
//! connects it to the editor and to the worker.
use crate::cloud_home::{self, CloseDecision, CloudDoc, HomeCommand, HomeReply, HomeWorker};
use crate::cloud_home_ui::{Action, Busy, Home, Keys, Net};
use craft_cloud_sync::{Config, DownloadedRevision, Snapshot, SyncEvent, SyncService, SyncStatus};
use photocraft_doc::{DocId, Document, Layer, LayerContent, SmartSource};
use photocraft_ui_egui::PhotocraftApp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
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
/// How long closing a cloud document waits for its checkpoint before the normal prompt is used.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

enum Command {
    Save {
        key: String,
        doc: Arc<Document>,
        revision: u64,
        token: String,
        source: Option<String>,
    },
    /// A [`Command::Save`] whose sender waits to learn whether the checkpoint reached the disk (closing a cloud document).
    Flush {
        key: String,
        doc: Arc<Document>,
        revision: u64,
        token: String,
        source: Option<String>,
        ack: mpsc::SyncSender<bool>,
    },
    Connect(Config),
    Open {
        path: PathBuf,
        key: String,
        binding: Option<(Uuid, Uuid)>,
    },
    Refresh,
    /// The project library: opens, renames, previews, pinning, the cache limit and recents.
    Home(HomeCommand),
    Stop,
}
enum Reply {
    Ready {
        endpoint: String,
        username: String,
        password: String,
        connected: bool,
        credential_notice: String,
        bindings: HashMap<String, String>,
        recents: Vec<(Uuid, u64)>,
        cache_limit: u64,
    },
    Saved {
        key: String,
        revision: u64,
        source: Option<String>,
        checkpoint: bool,
        blocked: bool,
        result: Result<(), String>,
        /// The project and revision the checkpoint was queued as.
        submission: Option<(Uuid, Uuid)>,
    },
    /// `catalog` holds one entry per head revision.
    State {
        status: SyncStatus,
        pending: usize,
        catalog: Vec<DownloadedRevision>,
        local: Vec<LocalCopy>,
        cache_bytes: u64,
        cache_limit: u64,
    },
    Opened {
        key: String,
        source: Option<String>,
        from_cloud: bool,
        binding: Option<(Uuid, Uuid)>,
        result: Result<Box<Document>, String>,
    },
    Connection {
        connected: bool,
        credential_notice: String,
    },
    Home(HomeReply),
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

/// What the editor's unsaved-changes prompt asks of the adapter (see `Services::discardable`).
struct CloseGate {
    docs: HashMap<DocId, GateDoc>,
    tx: mpsc::Sender<Command>,
    session_key: String,
    wait: Duration,
}
struct GateDoc {
    key: String,
    cloud: bool,
    durable: Option<u64>,
}
impl CloseGate {
    /// The documents whose current state is safe in a local checkpoint. All missing checkpoints
    /// are queued first, then share one `wait` budget for the whole Close All / Quit operation.
    fn allows(&mut self, states: &[&photocraft_engine::DocState]) -> Vec<DocId> {
        let started = Instant::now();
        let mut safe = Vec::new();
        let mut pending = Vec::new();
        for st in states {
            let Some(doc) = self.docs.get(&st.doc.id) else { continue };
            match cloud_home::close_decision(st.path.is_some(), doc.cloud, doc.durable, st.revision) {
                CloseDecision::Prompt => {}
                CloseDecision::Safe => safe.push(st.doc.id),
                CloseDecision::Flush => {
                let (ack, done) = mpsc::sync_channel(1);
                    let key = doc.key.clone();
                    if self
                        .tx
                        .send(Command::Flush {
                    key: doc.key.clone(),
                    doc: Arc::clone(&st.doc),
                    revision: st.revision,
                            token: sync_token(&self.session_key, &key, st.revision),
                    source: None,
                    ack,
                        })
                        .is_ok()
                    {
                        pending.push((st.doc.id, st.revision, done));
                    }
                }
            }
        }
        for (id, revision, done) in pending {
            let remaining = self.wait.saturating_sub(started.elapsed());
            if done.recv_timeout(remaining).unwrap_or(false) {
                if let Some(doc) = self.docs.get_mut(&id) {
                    doc.durable = Some(revision);
                }
                safe.push(id);
            }
        }
        safe
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
    /// The Account window.
    window: bool,
    connected: bool,
    status: SyncStatus,
    pending: usize,
    /// One entry per head revision, from the worker.
    catalog: Vec<DownloadedRevision>,
    local: Vec<LocalCopy>,
    error: Option<String>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The project library. Shared with the closure that draws it inside the editor's central area.
    home: Rc<RefCell<Home>>,
    gate: Rc<RefCell<CloseGate>>,
    /// Cloud identity of the open documents.
    cloud: HashMap<DocId, CloudDoc>,
    recents: cloud_home::Recents,
    pinned: BTreeSet<Uuid>,
    /// The working keys the worker was last told are open.
    sent_keys: HashSet<String>,
    cache_limit: u64,
    /// The limit the sync client reports it is applying (it lags a change by a poll).
    cache_in_effect: u64,
    cache_bytes: u64,
    /// Opens started from Home that wait for their document, by local key.
    opening: HashMap<String, Uuid>,
    /// Opens that, once the document exists, go on to Save a Copy.
    copy_after_open: HashSet<String>,
    /// Renames of closed projects in flight, by revision, with the title (kept for Retry).
    renaming: HashMap<Uuid, String>,
    /// The next untracked document without a file is the one Home's New is creating.
    awaiting_new: bool,
    listing_done: bool,
    sync_seen: bool,
    settled_polls: u32,
    cards_dirty: bool,
    open_titles: HashMap<Uuid, String>,
    shown_home_once: bool,
    /// Save a Copy is due: the document it is for was just opened.
    copy_now: bool,
    /// `home.*` requests from the control channel.
    control: Option<mpsc::Receiver<photocraft_ui_egui::ControlRequest>>,
}
impl DesktopApp {
    pub fn new(inner: PhotocraftApp, automation: Option<photocraft_automation::AuthorizedWorkspace>) -> Self {
        Self::new_in(inner, automation, crate::services::config_dir().map(|d| d.join("cloud-projects")))
    }

    /// [`Self::new`] with the adapter's own directory chosen by the caller (tests use a temporary one).
    fn new_in(inner: PhotocraftApp, automation: Option<photocraft_automation::AuthorizedWorkspace>, dir: Option<PathBuf>) -> Self {
        let mut inner = inner;
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
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
        let session_key = Uuid::new_v4().to_string();
        let home = Rc::new(RefCell::new(Home::default()));
        let gate = Rc::new(RefCell::new(CloseGate { docs: HashMap::new(), tx: tx.clone(), session_key: session_key.clone(), wait: FLUSH_WAIT }));
        {
            let view = Rc::clone(&home);
            inner.services.home_view = Some(Box::new(move |ui| {
                if let Ok(mut home) = view.try_borrow_mut() {
                    home.show(ui);
                }
            }));
            let gate = Rc::clone(&gate);
            inner.services.discardable = Some(Box::new(move |docs| gate.try_borrow_mut().map(|mut g| g.allows(docs)).unwrap_or_default()));
        }
        Self {
            inner,
            tx,
            rx,
            tracked: HashMap::new(),
            path_bindings: HashMap::new(),
            ready: false,
            session_key,
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
            home,
            gate,
            cloud: HashMap::new(),
            recents: cloud_home::Recents::default(),
            pinned: BTreeSet::new(),
            sent_keys: HashSet::new(),
            cache_limit: cloud_home::DEFAULT_CACHE_LIMIT,
            cache_in_effect: 0,
            cache_bytes: 0,
            opening: HashMap::new(),
            copy_after_open: HashSet::new(),
            renaming: HashMap::new(),
            awaiting_new: false,
            listing_done: false,
            sync_seen: false,
            settled_polls: 0,
            cards_dirty: true,
            open_titles: HashMap::new(),
            shown_home_once: false,
            copy_now: false,
            control: None,
        }
    }
    fn tick(&mut self, ctx: &egui::Context) {
        let mut new_projects: Vec<Uuid> = Vec::new();
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
                Reply::Ready { endpoint, username, password, connected, credential_notice, bindings, recents, cache_limit } => {
                    self.ready = true;
                    self.path_bindings = bindings;
                    self.endpoint = endpoint;
                    self.username = username;
                    self.password = password;
                    self.connected = connected;
                    // With no account there is no remote catalog to wait for. `Ready` can arrive
                    // before the worker's first state poll, so make the empty library usable now.
                    self.listing_done = !connected;
                    self.credential_notice = credential_notice;
                    self.recents = cloud_home::Recents::from_entries(recents);
                    self.cache_limit = cache_limit;
                    self.cards_dirty = true;
                    // The library is the start of the app: also when the Home screen preference is off.
                    if !self.shown_home_once && self.inner.session.documents().is_empty() {
                        self.inner.ui.chrome.home = Some(0);
                    }
                    self.shown_home_once = true;
                }
                Reply::Saved { key, revision, source, checkpoint, blocked, result, submission } => {
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
                    if let Some((project, version)) = submission {
                        for (id, t) in &self.tracked {
                            if t.key != key {
                                continue;
                            }
                            if let Some(doc) = self.cloud.get_mut(id) {
                                if doc.project.is_none() {
                                    new_projects.push(project);
                                }
                                doc.project = Some(project);
                                doc.version = Some(version);
                            }
                        }
                    }
                    if let Err(e) = result {
                        self.error = Some(e);
                    } else if self.tracked.values().all(|t| !t.blocked && t.queued == Some(t.observed)) {
                        self.error = None;
                    }
                }
                Reply::State { status, pending, catalog, local, cache_bytes, cache_limit } => {
                    if matches!(status, SyncStatus::Idle) && pending == 0 && self.tracked.values().all(|t| !t.blocked && t.queued == Some(t.observed)) {
                        self.error = None;
                    }
                    if matches!(status, SyncStatus::Syncing) {
                        self.sync_seen = true;
                        self.settled_polls = 0;
                    } else {
                        self.settled_polls = self.settled_polls.saturating_add(1);
                    }
                    self.listing_done = !self.connected || (self.sync_seen && !matches!(status, SyncStatus::Syncing)) || self.settled_polls >= 3;
                    self.status = status;
                    self.pending = pending;
                    self.catalog = catalog;
                    self.local = local;
                    self.cache_bytes = cache_bytes;
                    self.cache_in_effect = cache_limit;
                    self.cards_dirty = true;
                }
                Reply::Opened { key, source, from_cloud, binding, result } => {
                    let from_home = self.opening.remove(&key);
                    match result {
                        Ok(doc) => {
                            if let Some(index) = self.inner.session.documents().iter().position(|d| self.tracked.get(&d.doc.id).is_some_and(|t| t.key == key)) {
                                self.inner.session.set_active(index);
                                self.set_busy(from_home, None);
                                continue;
                            }
                            let index = open_native_document(&mut self.inner, *doc);
                            if let Some(d) = self.inner.session.documents().get(index) {
                                let now = Instant::now();
                                self.cloud.insert(d.doc.id, CloudDoc { project: binding.map(|b| b.0), version: binding.map(|b| b.1), cloud: from_cloud });
                                self.tracked.insert(
                                    d.doc.id,
                                    Tracked {
                                        key: key.clone(),
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
                            self.set_busy(from_home, None);
                            new_projects.extend(binding.map(|b| b.0));
                            if self.copy_after_open.remove(&key) {
                                self.copy_now = true;
                            }
                        }
                        Err(e) => {
                            self.copy_after_open.remove(&key);
                            match from_home {
                                Some(version) => self.set_busy(Some(version), Some(Busy::Failed { error: e, rename: None })),
                                None => self.error = Some(e),
                            }
                        }
                    }
                }
                Reply::Connection { connected, credential_notice } => {
                    self.connected = connected;
                    self.credential_notice = credential_notice;
                    self.listing_done = !connected;
                    self.sync_seen = false;
                    self.settled_polls = 0;
                    self.cards_dirty = true;
                }
                Reply::Home(HomeReply::Failed { version, error }) => {
                    if version.is_nil() {
                        self.error = Some(error);
                    } else {
                        self.opening.retain(|_, v| *v != version);
                        let rename = self.renaming.remove(&version);
                        self.set_busy(Some(version), Some(Busy::Failed { error, rename }));
                    }
                    self.cards_dirty = true;
                }
                Reply::Home(HomeReply::Renamed { version, new_version }) => {
                    log::debug!("project revision {version} renamed as {new_version}");
                    self.renaming.remove(&version);
                    self.set_busy(Some(version), None);
                    self.cards_dirty = true;
                }
                Reply::Home(HomeReply::Previews(results)) => {
                    if let Ok(mut home) = self.home.try_borrow_mut() {
                        home.previews.accept(ctx, results);
                    }
                }
                Reply::Error(e) => self.error = Some(e),
            }
        }
        for project in new_projects {
            self.touch_recent(project);
        }
        if !self.ready {
            ctx.request_repaint_after(Duration::from_millis(250));
            self.sync_home(ctx);
            return;
        }
        let now = Instant::now();
        for d in self.inner.session.documents() {
            let fresh = !self.tracked.contains_key(&d.doc.id);
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
            if fresh {
                // A document without a file that Home's New just made belongs to the cloud; so does nothing else we did not open.
                let from_home = self.awaiting_new && d.path.is_none();
                self.awaiting_new &= !from_home;
                self.cloud.entry(d.doc.id).or_insert(CloudDoc { project: None, version: None, cloud: from_home });
            }
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
        // A document that stopped being new (the dialog was cancelled) must not claim the next one.
        if self.awaiting_new && self.inner.ui.dialogs.is_empty() {
            self.awaiting_new = false;
        }
        let closed: Vec<_> = self.tracked.keys().filter(|id| !self.inner.session.documents().iter().any(|d| d.doc.id == **id)).copied().collect();
        for id in closed {
            self.cloud.remove(&id);
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
        if let Ok(mut gate) = self.gate.try_borrow_mut() {
            gate.docs = self
                .tracked
                .iter()
                .map(|(id, t)| (*id, GateDoc { key: t.key.clone(), cloud: self.cloud.get(id).is_some_and(|c| c.cloud), durable: t.durable }))
                .collect();
        }
        let keys: HashSet<String> = self.tracked.values().map(|t| t.key.clone()).collect();
        if keys != self.sent_keys {
            self.sent_keys = keys.clone();
            let _ = self.tx.send(Command::Home(HomeCommand::OpenKeys(keys)));
        }
        let pinned = cloud_home::pinned_set(self.cloud.values());
        if pinned != self.pinned {
            self.pinned = pinned.clone();
            let _ = self.tx.send(Command::Home(HomeCommand::Pin(pinned)));
        }
        self.sync_home(ctx);
        self.drain_control(ctx);
        if std::mem::take(&mut self.copy_now) {
            self.invoke_save_a_copy(ctx);
        }
        // Required even while typing continuously or while the window is otherwise idle.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    /// Feeds the library its model for this frame, sends the previews it wants decoded and carries out what it was asked.
    fn sync_home(&mut self, ctx: &egui::Context) {
        let home = Rc::clone(&self.home);
        let actions = {
            let Ok(mut home) = home.try_borrow_mut() else { return };
            let mut titles = HashMap::new();
            for d in self.inner.session.documents() {
                if let Some(project) = self.cloud.get(&d.doc.id).and_then(|c| c.project) {
                    titles.insert(project, d.doc.name.clone());
                }
            }
            if titles != self.open_titles {
                self.open_titles = titles;
                self.cards_dirty = true;
            }
            let net = match &self.status {
                SyncStatus::Idle => Net::Idle,
                SyncStatus::Syncing => Net::Syncing,
                SyncStatus::Offline => Net::Offline,
                SyncStatus::Error(e) => Net::Error(e.clone()),
            };
            let model = &mut home.model;
            model.ready = self.ready;
            model.connected = self.connected;
            model.net = net;
            model.listing_done = self.listing_done;
            model.error = self.error.clone();
            model.recents = self.recents.ids();
            model.cache_bytes = self.cache_bytes;
            model.cache_limit = if self.cache_in_effect == 0 { self.cache_limit } else { self.cache_in_effect };
            model.documents_open = !self.inner.session.documents().is_empty();
            model.now_ms = cloud_home::now_ms();
            if std::mem::take(&mut self.cards_dirty) {
                let requested: HashSet<Uuid> = model.busy.iter().filter(|(_, b)| matches!(b, Busy::Opening | Busy::Renaming)).map(|(v, _)| *v).collect();
                model.cards = cloud_home::cards_from_catalog(&self.catalog, &requested, &self.open_titles);
            }
            let batch = home.previews.take_batch();
            if !batch.is_empty() {
                let _ = self.tx.send(Command::Home(HomeCommand::Previews(batch)));
            }
            home.take_actions()
        };
        for action in actions {
            self.perform(ctx, action);
        }
    }

    fn set_busy(&mut self, version: Option<Uuid>, busy: Option<Busy>) {
        let Some(version) = version else { return };
        if let Ok(mut home) = self.home.try_borrow_mut() {
            match busy {
                Some(b) => home.model.busy.insert(version, b),
                None => home.model.busy.remove(&version),
            };
        }
        self.cards_dirty = true;
    }

    /// The open document represented by a card. A conflict card selects only the document bound to
    /// that exact revision; falling back to the project is safe only while it has a single head.
    fn index_for_card(&self, project: Uuid, version: Uuid) -> Option<usize> {
        let documents = self.inner.session.documents();
        let exact = documents.iter().position(|d| self.cloud.get(&d.doc.id).and_then(|c| c.version) == Some(version));
        if exact.is_some() {
            return exact;
        }
        let heads: HashSet<Uuid> = self
            .catalog
            .iter()
            .filter(|revision| revision.project_id == project && revision.heads.contains(&revision.version_id))
            .map(|revision| revision.version_id)
            .collect();
        if heads.len() != 1 {
            return None;
        }
        documents.iter().position(|d| self.cloud.get(&d.doc.id).and_then(|c| c.project) == Some(project))
    }

    fn touch_recent(&mut self, project: Uuid) {
        self.recents.touch(project, cloud_home::now_ms());
        let _ = self.tx.send(Command::Home(HomeCommand::Recents(self.recents.entries().to_vec())));
    }

    /// Whether the library is what the central area shows right now.
    fn home_visible(&mut self) -> bool {
        let documents = self.inner.session.documents().len();
        let auto = self.inner.session.prefs().general.auto_show_home_screen;
        self.inner.ui.chrome.shows_home(documents, auto)
    }

    fn leave_home(&mut self) {
        if !self.inner.session.documents().is_empty() {
            self.inner.ui.chrome.home = None;
        }
    }

    fn invoke_save_a_copy(&mut self, ctx: &egui::Context) {
        match photocraft_ui_egui::menus::invoke(&mut self.inner, ctx, "file.saveACopy", json!({})) {
            Ok(_) => {}
            Err(e) if e == "cancelled" => {}
            Err(e) => {
                self.inner.ui.status = e;
                self.inner.ui.status_error = true;
            }
        }
    }

    fn perform(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Open { project, version } => self.open_project(project, version, false),
            Action::Rename { project, version, title } => self.rename_project(project, version, title),
            Action::SaveCopy { project, version } => match self.index_for_card(project, version) {
                Some(index) => {
                    self.inner.session.set_active(index);
                    self.invoke_save_a_copy(ctx);
                }
                None => self.open_project(project, version, true),
            },
            Action::New => {
                self.awaiting_new = true;
                if let Err(e) = photocraft_ui_egui::menus::invoke(&mut self.inner, ctx, "file.new", json!({})) {
                    self.awaiting_new = false;
                    self.inner.ui.status = e;
                    self.inner.ui.status_error = true;
                }
            }
            Action::Back => self.leave_home(),
            Action::Account => {
                self.window = true;
                let _ = self.tx.send(Command::Refresh);
            }
            Action::RetrySync => {
                self.error = None;
                for t in self.tracked.values_mut() {
                    t.blocked = false;
                }
                let _ = self.tx.send(Command::Refresh);
            }
        }
    }

    /// Opens a project: the document that is already open for it, else the revision (downloaded first if it is only in the cloud).
    fn open_project(&mut self, project: Uuid, version: Uuid, then_copy: bool) {
        if let Some(index) = self.index_for_card(project, version) {
            self.inner.session.set_active(index);
            self.leave_home();
            self.touch_recent(project);
            return;
        }
        let waiting = self.home.try_borrow().map(|h| matches!(h.model.busy.get(&version), Some(Busy::Opening | Busy::Renaming))).unwrap_or(false);
        if waiting {
            return;
        }
        let key = format!("cloud-{}", Uuid::new_v4());
        self.opening.insert(key.clone(), version);
        if then_copy {
            self.copy_after_open.insert(key.clone());
        }
        self.set_busy(Some(version), Some(Busy::Opening));
        if self.tx.send(Command::Home(HomeCommand::OpenVersion { project, version, key: key.clone() })).is_err() {
            self.opening.remove(&key);
            self.copy_after_open.remove(&key);
            self.set_busy(Some(version), Some(Busy::Failed { error: "The cloud worker stopped; restart PhotoCraft.".into(), rename: None }));
        }
    }

    /// Renames a project: the open document (the next autosave carries the name), else a renamed revision.
    fn rename_project(&mut self, project: Uuid, version: Uuid, title: String) {
        let title = match cloud_home::validate_title(&title) {
            Ok(title) => title,
            Err(why) => {
                self.set_busy(Some(version), Some(Busy::Failed { error: why.into(), rename: None }));
                return;
            }
        };
        if let Some(index) = self.index_for_card(project, version) {
            let previous = self.inner.session.active_index();
            self.inner.session.set_active(index);
            let renamed = self.inner.session.edit("Rename Document", |doc, _| {
                doc.name.clone_from(&title);
                Ok(())
            });
            if let Some(previous) = previous {
                self.inner.session.set_active(previous);
            }
            match renamed {
                Ok(()) => self.set_busy(Some(version), None),
                Err(e) => self.set_busy(Some(version), Some(Busy::Failed { error: e.to_string(), rename: Some(title) })),
            }
            self.cards_dirty = true;
            return;
        }
        self.renaming.insert(version, title.clone());
        self.set_busy(Some(version), Some(Busy::Renaming));
        if self.tx.send(Command::Home(HomeCommand::Rename { project, version, title: title.clone() })).is_err() {
            self.renaming.remove(&version);
            self.set_busy(Some(version), Some(Busy::Failed { error: "The cloud worker stopped; restart PhotoCraft.".into(), rename: Some(title) }));
        }
    }

    /// Takes the keys Home uses before the editor's shortcuts see them (Ctrl+F would be Last Filter, F2 Cut, Esc cancels).
    fn take_home_keys(&mut self, ctx: &egui::Context) {
        if !self.ready || !self.home_visible() || ctx.text_edit_focused() || self.window {
            return;
        }
        let modal = !self.inner.ui.dialogs.is_empty() || self.inner.file_dialog_open() || self.inner.discard_prompt_open();
        let keys = ctx.input_mut(|i| Keys {
            focus_search: i.consume_key(egui::Modifiers::COMMAND, egui::Key::F),
            rename: !modal && i.consume_key(egui::Modifiers::NONE, egui::Key::F2),
            leave: !modal && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
        });
        if let Ok(mut home) = self.home.try_borrow_mut() {
            home.keys = keys;
        }
    }

    /// Answers `home.*` control requests: read-only listing and open-by-version.
    fn drain_control(&mut self, ctx: &egui::Context) {
        let Some(control) = &self.control else { return };
        let requests: Vec<_> = std::iter::from_fn(|| control.try_recv().ok()).take(16).collect();
        for request in requests {
            let reply = if request.deadline.is_some_and(|d| Instant::now() > d) {
                json!({"ok": false, "error": "timeout: not run — the request was still queued when its deadline passed"})
            } else {
                self.control_call(ctx, &request.method, &request.params)
            };
            let _ = request.reply.send(reply);
        }
    }

    fn control_call(&mut self, ctx: &egui::Context, method: &str, params: &Value) -> Value {
        match method {
            "home.list" => {
                let Ok(home) = self.home.try_borrow() else { return json!({"ok": false, "error": "the library is busy; try again"}) };
                let net = match &self.status {
                    SyncStatus::Idle => "idle".to_owned(),
                    SyncStatus::Syncing => "syncing".to_owned(),
                    SyncStatus::Offline => "offline".to_owned(),
                    SyncStatus::Error(e) => format!("error: {e}"),
                };
                json!({"ok": true, "result": cloud_home::list_json(&home.model.cards, self.ready, self.connected, &net, cloud_home::now_ms())})
            }
            "home.open" => {
                let Some(version) = params.get("version").and_then(Value::as_str).and_then(|v| Uuid::parse_str(v).ok()) else {
                    return json!({"ok": false, "error": "`version` must be a revision id from home.list"});
                };
                let project = self.home.try_borrow().ok().and_then(|h| h.model.cards.iter().find(|c| c.version == version).map(|c| c.project));
                let Some(project) = project else { return json!({"ok": false, "error": "no project card has that version; call home.list first"}) };
                self.perform(ctx, Action::Open { project, version });
                json!({"ok": true, "result": {"requested": true, "project": project.to_string(), "version": version.to_string()}})
            }
            other => json!({"ok": false, "error": format!("unknown method `{other}`")}),
        }
    }

    /// Routes `home.*` control requests here (see `cloud_home::split_control`).
    pub fn with_control(mut self, control: mpsc::Receiver<photocraft_ui_egui::ControlRequest>) -> Self {
        self.control = Some(control);
        self
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let documents = self.inner.session.documents().len();
            let shown = self.home_visible();
            if ui.selectable_label(shown, "Home").on_hover_text("Your projects, saved to the cloud").clicked() {
                self.inner.ui.chrome.home = if shown && documents > 0 { None } else { Some(documents) };
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
        egui::Window::new("Account").open(&mut open).default_width(520.0).show(ui.ctx(), |ui| {
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
            ui.horizontal(|ui| {
                ui.label("Local cache limit");
                let mut limit = self.cache_limit;
                egui::ComboBox::from_id_salt("cloud-cache-limit").selected_text(preset_label(limit)).show_ui(ui, |ui| {
                    for preset in cloud_home::CACHE_PRESETS {
                        ui.selectable_value(&mut limit, preset, preset_label(preset));
                    }
                });
                if limit != self.cache_limit {
                    self.cache_limit = limit;
                    let _ = self.tx.send(Command::Home(HomeCommand::CacheLimit(limit)));
                }
            });
            ui.small(format!(
                "{}. Projects waiting to upload, open documents and conflict copies are never removed.",
                cloud_home::cache_label(self.cache_bytes, self.cache_limit)
            ));
            ui.separator();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
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
        self.take_home_keys(ctx);
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

/// "512 MB", "2 GB": a cache limit preset.
fn preset_label(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes >= GIB { format!("{} GB", bytes / GIB) } else { format!("{} MB", bytes / (1024 * 1024)) }
}

/// Background completion can arrive after the shell's per-frame view synchronization.
/// Install the document and its UI view together before chrome/canvas rendering begins.
fn open_native_document(app: &mut PhotocraftApp, document: Document) -> usize {
    let index = app.session.add_document(document, None);
    app.sync_views();
    index
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
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
            service
                .submit(Snapshot { local_key: key, title, extension: "pcraft".into(), local_token: token, native, preview: None })
                .map_err(|e| e.to_string())?;
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

/// A cloud open waiting for its revision payload to download.
struct AwaitedOpen {
    key: String,
    binding: (Uuid, Uuid),
}
fn take_awaited(waiting: &mut Vec<AwaitedOpen>, version: Uuid) -> Vec<AwaitedOpen> {
    let (done, rest) = std::mem::take(waiting).into_iter().partition(|open| open.binding.1 == version);
    *waiting = rest;
    done
}
fn open_project(
    path: &Path,
    key: String,
    binding: Option<(Uuid, Uuid)>,
    service: Option<&SyncService>,
    automation: Option<&photocraft_automation::AuthorizedWorkspace>,
    dir: &Path,
) -> Reply {
    let result = crate::crash_guard::guard("Open cloud project", || {
        let document = load_native_project_with_access(path, automation)?;
        if let Some((project, version)) = binding {
            service.ok_or("Sync queue unavailable; try opening the local working copy")?.bind(&key, project, version).map_err(|e| e.to_string())?;
        }
        Ok(document)
    });
    let source = std::fs::read(dir.join("working").join(format!("{key}.json")))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("source").and_then(Value::as_str).map(str::to_owned));
    Reply::Opened { key, source, from_cloud: binding.is_some(), binding, result: result.map(Box::new) }
}

/// What writing one checkpoint did.
struct Checkpointed {
    /// The native file and its metadata are on disk.
    durable: bool,
    /// Publishing is blocked until the document changes (a linked original is missing).
    blocked: bool,
    result: Result<(), String>,
    /// The project and revision the checkpoint was queued as.
    submission: Option<(Uuid, Uuid)>,
}

/// Serializes `doc` into a durable local checkpoint (with a bounded preview) and hands it to the sync queue.
#[allow(clippy::too_many_arguments)]
fn checkpoint(
    dir: &Path,
    service: Option<&SyncService>,
    automation: Option<&photocraft_automation::AuthorizedWorkspace>,
    home: &HomeWorker,
    key: &str,
    doc: &Arc<Document>,
    token: &str,
    source: Option<&str>,
) -> Checkpointed {
    let mut durable = false;
    let mut blocked = false;
    let mut submission = None;
    let result = crate::crash_guard::guard("Cloud autosave", || {
        let portable = portable_with_access(doc, source, automation);
        let checkpoint_doc = portable.as_ref().unwrap_or(doc);
        let native = photocraft_format::save_to_bytes(checkpoint_doc, &Default::default()).map_err(|e| e.to_string())?;
        let hash = format!("{:x}", Sha256::digest(&native));
        let local_token = format!("{token}:{hash}");
        let payload = format!("{key}-{hash}.pcraft");
        atomic_write(&dir.join("working").join(&payload), &native)?;
        let mut metadata = json!({"key":key,"title":doc.name,"source":source,"payload":payload,"sha256":hash,"local_token":local_token,"portable":portable.is_ok(),"needs_submit":portable.is_ok(),"portability_error":portable.as_ref().err()});
        let metadata_path = dir.join("working").join(format!("{key}.json"));
        atomic_write(&metadata_path, &serde_json::to_vec(&metadata).map_err(|e| e.to_string())?)?;
        durable = true;
        // The metadata now names this checkpoint: the ones it replaced are no longer needed.
        cloud_home::prune_superseded(&dir.join("working"), key, &payload);
        if let Err(error) = portable {
            blocked = true;
            return Err(error);
        }
        let service = service.ok_or("Saved locally; sync queue unavailable")?;
        // The checkpoint is already safe on disk; the preview is a courtesy and gives up after a moment.
        let preview = home.preview(doc);
        let queued = service
            .submit(Snapshot { local_key: key.to_owned(), title: doc.name.clone(), extension: "pcraft".into(), local_token, native, preview })
            .map_err(|e| e.to_string())?;
        submission = Some((queued.project_id, queued.version_id));
        metadata["needs_submit"] = json!(false);
        atomic_write(&metadata_path, &serde_json::to_vec(&metadata).map_err(|e| e.to_string())?)?;
        Ok(())
    });
    Checkpointed { durable, blocked, result, submission }
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
    let (mut home, startup) = HomeWorker::new(&dir);
    let _ = replies.send(Reply::Ready {
        bindings,
        endpoint,
        username,
        password,
        connected,
        credential_notice,
        recents: startup.recents,
        cache_limit: startup.cache_limit,
    });
    let mut service = match SyncService::new(config, dir.join("sync"), "photocraft") {
        Ok(s) => Some(s),
        Err(e) => {
            let _ = replies.send(Reply::Error(format!("Sync queue unavailable; local checkpoints continue: {e}")));
            None
        }
    };
    if let Some(service) = &service {
        home.apply(service);
    }
    let mut next_retry = Instant::now();
    let mut awaiting: Vec<AwaitedOpen> = Vec::new();
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
                let done = checkpoint(&dir, service.as_ref(), automation.as_ref(), &home, &key, &doc, &token, source.as_deref());
                let _ = replies.send(Reply::Saved {
                    key,
                    revision,
                    source,
                    checkpoint: done.durable,
                    blocked: done.blocked,
                    result: done.result,
                    submission: done.submission,
                });
            }
            Ok(Command::Flush { key, doc, revision, token, source, ack }) => {
                let done = checkpoint(&dir, service.as_ref(), automation.as_ref(), &home, &key, &doc, &token, source.as_deref());
                let _ = ack.send(done.durable);
                let _ = replies.send(Reply::Saved {
                    key,
                    revision,
                    source,
                    checkpoint: done.durable,
                    blocked: done.blocked,
                    result: done.result,
                    submission: done.submission,
                });
            }
            Ok(Command::Connect(config)) => {
                let credentials = config.clone();
                let public = json!({"endpoint":config.endpoint,"username":config.username});
                if let Some(service) = &mut service {
                    service.shutdown();
                }
                if !awaiting.is_empty() {
                    awaiting.clear();
                    let _ = replies.send(Reply::Error("Cloud download interrupted by reconnecting; open the project again".into()));
                }
                for reply in home.interrupt() {
                    let _ = replies.send(Reply::Home(reply));
                }
                match SyncService::new(config, dir.join("sync"), "photocraft") {
                    Ok(s) => {
                        home.apply(&s);
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
                            Ok(local) => {
                                home.apply(&local);
                                service = Some(local);
                            }
                            Err(local_error) => {
                                service = None;
                                let _ = replies
                                    .send(Reply::Error(format!("Cloud connection failed; local checkpoints continue, sync queue unavailable: {local_error}")));
                            }
                        }
                    }
                }
            }
            Ok(Command::Open { path, key, binding }) => match (binding, service.as_ref()) {
                (Some(bound), Some(sync)) => {
                    // The revision may not be cached: ask for the payload and finish the open when it arrives.
                    awaiting.push(AwaitedOpen { key, binding: bound });
                    sync.request_payload(bound.1);
                }
                _ => {
                    let _ = replies.send(open_project(&path, key, binding, service.as_ref(), automation.as_ref(), &dir));
                }
            },
            Ok(Command::Refresh) => {
                if let Some(service) = &service {
                    service.request_sync();
                }
            }
            Ok(Command::Home(command)) => {
                for reply in home.handle(command, service.as_ref()) {
                    let _ = replies.send(Reply::Home(reply));
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
            match event {
                SyncEvent::Error(message) => {
                    let _ = replies.send(Reply::Error(message));
                }
                SyncEvent::Fetched { version_id, path, .. } => {
                    // Payloads download on demand; finish the opens that were waiting for this one.
                    for open in take_awaited(&mut awaiting, version_id) {
                        let _ = replies.send(open_project(&path, open.key, Some(open.binding), service.as_ref(), automation.as_ref(), &dir));
                    }
                    let (opens, replies_home) = home.fetched(version_id, service.as_ref());
                    for open in opens {
                        let _ = replies.send(open_project(&path, open.key, Some((open.project, open.version)), service.as_ref(), automation.as_ref(), &dir));
                    }
                    for reply in replies_home {
                        let _ = replies.send(Reply::Home(reply));
                    }
                }
                SyncEvent::Published { local_token, .. } => {
                    if let Some(key) = home.published(&local_token) {
                        log::debug!("working copy {key} is published and closed: removed");
                    }
                }
                SyncEvent::FetchFailed { version_id, error } => {
                    if !take_awaited(&mut awaiting, version_id).is_empty() {
                        let _ = replies.send(Reply::Error(format!("Cannot open the cloud project: {error}")));
                    }
                    for reply in home.fetch_failed(version_id, &error) {
                        let _ = replies.send(Reply::Home(reply));
                    }
                }
                _ => {}
            }
        }
        // The library lists heads only; older revisions stay in the shared client.
        let heads: Vec<_> = state.catalog.into_iter().filter(|r| r.heads.contains(&r.version_id)).collect();
        let _ = replies.send(Reply::State {
            status: state.status,
            pending: state.pending,
            catalog: heads,
            local: local_copies(&dir.join("working")),
            cache_bytes: state.cache_bytes,
            cache_limit: state.cache_limit,
        });
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
    #[test]
    fn cloud_open_waits_for_the_on_demand_payload() {
        let open = |n: u8| AwaitedOpen { key: format!("cloud-{n}"), binding: (Uuid::from_u128(1), Uuid::from_u128(u128::from(n))) };
        let mut waiting = vec![open(1), open(2), open(1)];
        let taken = take_awaited(&mut waiting, Uuid::from_u128(1));
        assert_eq!(taken.len(), 2);
        assert_eq!(waiting.len(), 1);
        assert!(take_awaited(&mut waiting, Uuid::from_u128(9)).is_empty());
        let dir = std::env::temp_dir().join(format!("photocraft-cloud-lazy-{}", Uuid::new_v4()));
        let (tx, commands) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let worker_dir = dir.clone();
        let handle = std::thread::spawn(move || worker(commands, replies, worker_dir, None));
        tx.send(Command::Save { key: "doc".into(), doc: Arc::new(doc()), revision: 1, token: "lazy-open".into(), source: None }).unwrap();
        let revision = loop {
            if let Reply::State { catalog, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap()
                && let Some(revision) = catalog.into_iter().next()
            {
                break revision;
            }
        };
        // A cached revision completes through the sync service's Fetched event and is bound for continued editing.
        tx.send(Command::Open { path: revision.local_path.clone(), key: "cloud-open".into(), binding: Some((revision.project_id, revision.version_id)) })
            .unwrap();
        loop {
            if let Reply::Opened { key, from_cloud, result, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                assert!(key == "cloud-open" && from_cloud);
                result.unwrap();
                break;
            }
        }
        // A revision the service does not know is reported as a failed download, not as a missing file.
        tx.send(Command::Open { path: dir.join("sync/payloads/absent"), key: "cloud-absent".into(), binding: Some((revision.project_id, Uuid::new_v4())) })
            .unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Reply::Error(e) if e.contains("Cannot open the cloud project") => {
                    assert!(e.contains("not in the catalog"), "{e}");
                    break;
                }
                Reply::Opened { .. } => panic!("opened a revision without a payload"),
                _ => {}
            }
        }
        tx.send(Command::Stop).unwrap();
        handle.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod home_tests;
