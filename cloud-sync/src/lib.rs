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
    time::Duration,
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
const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_LIST: u64 = 4 * 1024 * 1024;
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
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub project_id: Uuid,
    pub version_id: Uuid,
}
#[derive(Clone, Debug)]
pub struct DownloadedRevision {
    pub project_id: Uuid,
    pub version_id: Uuid,
    pub parents: Vec<Uuid>,
    pub heads: Vec<Uuid>,
    pub title: String,
    pub extension: String,
    pub local_path: PathBuf,
}
#[derive(Clone, Debug)]
pub enum SyncEvent {
    Durable { project_id: Uuid, version_id: Uuid, local_token: String },
    Published { project_id: Uuid, version_id: Uuid, local_token: String },
    Downloaded(DownloadedRevision),
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
#[derive(Clone, Debug, Default)]
pub struct SyncState {
    pub status: SyncStatus,
    pub pending: usize,
    pub catalog: Vec<DownloadedRevision>,
    pub events: Vec<SyncEvent>,
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
}
struct Shared {
    journal: Journal,
    state: SyncState,
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
        let j = if cache.join("journal.json").exists() {
            serde_json::from_slice(&bounded_file(&cache.join("journal.json"), 16 * 1024 * 1024)?)?
        } else {
            Journal::default()
        };
        let mut all = BTreeMap::new();
        for entry in fs::read_dir(cache.join("manifests"))? {
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) == Some("json") {
                let m: Manifest = serde_json::from_slice(&bounded_file(&path, MAX_MANIFEST)?)?;
                validate(&m, app)?;
                all.insert(m.version, Pending { manifest: m, local_token: String::new() });
            }
        }
        for p in &j.pending {
            validate(&p.manifest, app)?;
            all.insert(p.manifest.version, p.clone());
        }
        for p in all.values() {
            let m = &p.manifest;
            let b = bounded_file(&payload(&cache, m.version), MAX_PAYLOAD)?;
            if b.len() as u64 != m.size || hash(&b) != m.sha256 {
                return Err(Error("cached revision payload checksum mismatch".into()));
            }
        }
        let initial = local_catalog(&cache, &all.into_values().collect::<Vec<_>>());
        let shared = Arc::new(Mutex::new(Shared { journal: j, state: SyncState { catalog: initial, ..SyncState::default() } }));
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
                    lock(&s).state.status = SyncStatus::Offline;
                }
                {
                    let mut guard = lock(&s);
                    let count = guard.state.events.len();
                    if count > 256 {
                        guard.state.events.drain(..count - 256);
                    }
                }
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
    pub fn poll(&self) -> SyncState {
        let mut s = lock(&self.shared);
        SyncState {
            status: s.state.status.clone(),
            pending: s.journal.pending.len(),
            catalog: s.state.catalog.clone(),
            events: std::mem::take(&mut s.state.events),
        }
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
        };
        atomic(&payload(&self.cache, version), &s.native)?;
        atomic(&self.cache.join("manifests").join(format!("{version}.json")), &serde_json::to_vec(&m)?)?;
        let mut next = state.journal.clone();
        next.receipts
            .entry(s.local_key.clone())
            .or_default()
            .insert(s.local_token.clone(), Receipt { project, version, sha256: m.sha256.clone(), title: m.title.clone(), extension: m.extension.clone() });
        next.pending.push(Pending { manifest: m, local_token: s.local_token.clone() });
        next.bindings.insert(s.local_key, (project, version));
        journal(&self.cache, &next)?;
        state.journal = next;
        let local = local_catalog(&self.cache, &state.journal.pending);
        for item in local {
            if !state.state.catalog.iter().any(|v| v.version_id == item.version_id) {
                state.state.catalog.push(item);
            }
        }
        refresh_heads(&mut state.state.catalog);
        state.state.events.push(SyncEvent::Durable { project_id: project, version_id: version, local_token: s.local_token });
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
    {
        return Err(Error("invalid revision manifest".into()));
    }
    Ok(())
}
fn cycle(r: &Remote, c: &Path, app: &str, s: &Mutex<Shared>, stop: &AtomicBool) -> Result<()> {
    // Flat UUID filenames avoid untrusted path interpretation and intermediate collection requirements.
    let pending = lock(s).journal.pending.clone();
    for p in pending {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        validate(&p.manifest, app)?;
        let b = bounded_file(&payload(c, p.manifest.version), MAX_PAYLOAD)?;
        if b.len() as u64 != p.manifest.size || hash(&b) != p.manifest.sha256 {
            return Err(Error("local outbox checksum mismatch".into()));
        }
        r.immutable(&format!("{}.payload", p.manifest.version), &b, MAX_PAYLOAD)?;
        r.immutable(&format!("{}.json", p.manifest.version), &serde_json::to_vec(&p.manifest)?, MAX_MANIFEST)?;
        let mut shared = lock(s);
        let mut next = shared.journal.clone();
        next.pending.retain(|x| x.manifest.version != p.manifest.version);
        journal(c, &next)?;
        shared.journal = next;
        shared.state.events.push(SyncEvent::Published { project_id: p.manifest.project, version_id: p.manifest.version, local_token: p.local_token });
    }
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
                    return Err(Error("published remote revision changed immutable metadata".into()));
                }
            }
            manifests.push(m);
        }
    }
    validate_dag(&manifests)?;
    let mut parents = BTreeSet::new();
    for m in &manifests {
        for p in &m.parents {
            if manifests.iter().any(|v| v.version == *p && v.project != m.project) {
                return Err(Error("revision parent belongs to another project".into()));
            }
            parents.insert(*p);
        }
    }
    let mut catalog = Vec::new();
    for m in &manifests {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let path = payload(c, m.version);
        let good = bounded_file(&path, MAX_PAYLOAD).map(|b| b.len() as u64 == m.size && hash(&b) == m.sha256).unwrap_or(false);
        if !good {
            let b = r.get(&format!("{}.payload", m.version), MAX_PAYLOAD)?.ok_or_else(|| Error("published revision payload missing".into()))?;
            if b.len() as u64 != m.size || hash(&b) != m.sha256 {
                return Err(Error("download checksum mismatch".into()));
            }
            atomic(&path, &b)?;
        }
        atomic(&c.join("manifests").join(format!("{}.json", m.version)), &serde_json::to_vec(m)?)?;
        let item = DownloadedRevision {
            project_id: m.project,
            version_id: m.version,
            parents: m.parents.clone(),
            heads: manifests.iter().filter(|v| v.project == m.project && !parents.contains(&v.version)).map(|v| v.version).collect(),
            title: m.title.clone(),
            extension: m.extension.clone(),
            local_path: path,
        };
        catalog.push(item);
    }
    let mut shared = lock(s);
    for item in &catalog {
        if !shared.state.catalog.iter().any(|v| v.version_id == item.version_id) {
            shared.state.events.push(SyncEvent::Downloaded(item.clone()));
        }
    }
    for item in &shared.state.catalog {
        if !catalog.iter().any(|v| v.version_id == item.version_id) {
            catalog.push(item.clone());
        }
    }
    refresh_heads(&mut catalog);
    shared.state.catalog = catalog;
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

fn local_catalog(c: &Path, pending: &[Pending]) -> Vec<DownloadedRevision> {
    let parents: BTreeSet<_> = pending.iter().flat_map(|p| p.manifest.parents.iter().copied()).collect();
    pending
        .iter()
        .map(|p| {
            let m = &p.manifest;
            DownloadedRevision {
                project_id: m.project,
                version_id: m.version,
                parents: m.parents.clone(),
                heads: pending
                    .iter()
                    .filter(|p| p.manifest.project == m.project && !parents.contains(&p.manifest.version))
                    .map(|p| p.manifest.version)
                    .collect(),
                title: m.title.clone(),
                extension: m.extension.clone(),
                local_path: payload(c, m.version),
            }
        })
        .collect()
}

fn refresh_heads(catalog: &mut [DownloadedRevision]) {
    let parents: BTreeSet<_> = catalog.iter().flat_map(|r| r.parents.iter().copied()).collect();
    let heads: Vec<_> = catalog.iter().filter(|r| !parents.contains(&r.version_id)).map(|r| (r.project_id, r.version_id)).collect();
    for item in catalog {
        item.heads = heads.iter().filter(|(p, _)| *p == item.project_id).map(|(_, v)| *v).collect();
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
    }
    #[test]
    fn cache_lock_corruption_and_account_scope() {
        let d = Dav::new();
        let c = tempfile::tempdir().unwrap();
        let mut s = offline(c.path());
        assert!(SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, c.path(), "photocraft").is_err());
        let sub = s.submit(snap("k")).unwrap();
        let handle = s.submitter();
        s.shutdown();
        assert!(handle.submit(snap("old")).is_err());
        let mut s = SyncService::new(Config { endpoint: d.endpoint.clone(), username: "one".into(), password: "p".into() }, c.path(), "photocraft").unwrap();
        s.shutdown();
        assert!(SyncService::new(Config { endpoint: d.endpoint.clone(), username: "two".into(), password: "p".into() }, c.path(), "photocraft").is_err());
        fs::write(payload(c.path(), sub.version_id), b"damaged").unwrap();
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
        let state = wait(&b, |s| s.catalog.iter().any(|r| r.version_id == base.version_id));
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
        let state = wait(&b, |s| s.catalog.iter().any(|r| r.version_id == saved.version_id));
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
        assert_eq!(fs::read(&cat[0].local_path).unwrap(), b"native bytes");
        fs::remove_file(&cat[0].local_path).unwrap();
        lock(&d.objects).insert(format!("{}.payload", sub.version_id), b"corrupt".to_vec());
        assert!(cycle(&r, target.path(), "photocraft", &other.shared, &AtomicBool::new(false)).is_err());
        assert!(!cat[0].local_path.exists());
    }
}
