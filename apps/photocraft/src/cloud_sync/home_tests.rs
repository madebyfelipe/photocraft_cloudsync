//! The project library wired into the adapter: a real worker thread, a real sync queue (no server) and
//! a temporary adapter directory, driven through `tick` the way the frame loop does.
use super::*;
use crate::cloud_home_ui::Busy;
use eframe::App as _;
use photocraft_ui_egui::menus;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("photocraft-home-app-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn desktop(dir: &Path) -> DesktopApp {
    desktop_with(dir, Default::default())
}

fn desktop_with(dir: &Path, services: photocraft_ui_egui::Services) -> DesktopApp {
    let app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
    DesktopApp::new_in(app, None, Some(dir.to_path_buf()))
}

fn pump(app: &mut DesktopApp, ctx: &egui::Context, what: &str, until: impl Fn(&mut DesktopApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.tick(ctx);
        if until(app) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Makes every document's autosave due now instead of after the debounce.
fn due(app: &mut DesktopApp) {
    let past = |t: Instant| t.checked_sub(Duration::from_secs(30)).unwrap_or(t);
    for t in app.tracked.values_mut() {
        t.changed = past(t.changed);
        t.first_change = past(t.first_change);
    }
}

fn model<R>(app: &DesktopApp, f: impl FnOnce(&crate::cloud_home_ui::Model) -> R) -> R {
    f(&app.home.borrow().model)
}

fn ready(dir: &Path) -> (DesktopApp, egui::Context) {
    let mut app = desktop(dir);
    let ctx = egui::Context::default();
    pump(&mut app, &ctx, "the worker", |a| a.ready);
    (app, ctx)
}

/// Creates a document the way Home's New does (the dialog's OK runs `file.new`) and waits until it
/// is checkpointed, queued and listed as a card.
fn new_project(app: &mut DesktopApp, ctx: &egui::Context) -> (Uuid, Uuid) {
    app.perform(ctx, Action::New);
    assert!(app.awaiting_new, "New waits for the dialog's document");
    assert_eq!(app.inner.ui.dialogs.len(), 1, "the normal New Document dialog is shown");
    app.inner.run("file.new", json!({"width": 64, "height": 48, "name": "Home project"})).unwrap();
    app.tick(ctx);
    let id = app.inner.session.active().unwrap().doc.id;
    assert!(app.cloud.get(&id).is_some_and(|c| c.cloud), "a document made from Home belongs to the cloud");
    assert!(!app.awaiting_new);
    app.inner.ui.dialogs.clear();
    settle(app, ctx);
    let card = model(app, |m| m.cards.first().cloned()).unwrap();
    (card.project, card.version)
}

/// Waits until every open document is durable and the queue lists the newest revision as a card.
fn settle(app: &mut DesktopApp, ctx: &egui::Context) {
    due(app);
    pump(app, ctx, "the checkpoint", |a| {
        a.tracked.values().all(|t| t.durable == Some(t.observed) && t.queued == Some(t.observed)) && a.cloud.values().all(|c| c.project.is_some())
    });
    let want = app.inner.session.documents().iter().filter_map(|d| app.cloud.get(&d.doc.id).and_then(|c| c.version)).collect::<Vec<_>>();
    pump(app, ctx, "the card", |a| want.iter().all(|v| model(a, |m| m.cards.iter().any(|c| c.version == *v))));
}

fn listing(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn catalog_head(project: Uuid, version: Uuid, heads: &[Uuid], title: &str, path: PathBuf) -> DownloadedRevision {
    DownloadedRevision {
        project_id: project,
        version_id: version,
        parents: Vec::new(),
        heads: heads.to_vec(),
        title: title.into(),
        extension: "pcraft".into(),
        local_path: path,
        created_ms: Some(1),
        size: 1,
        preview_path: None,
        cached: true,
        pending: false,
    }
}

#[test]
fn new_from_home_autosaves_into_the_adapter_directory_only() {
    let dir = temp("new");
    let here = std::env::current_dir().unwrap();
    let before = std::fs::read_dir(&here).unwrap().flatten().map(|e| e.path()).collect::<std::collections::BTreeSet<_>>();
    let (mut app, ctx) = ready(&dir);
    let (project, version) = new_project(&mut app, &ctx);
    let doc = app.inner.session.active().unwrap();
    assert!(doc.path.is_none(), "a Home document has no file of its own");
    let card = model(&app, |m| m.cards[0].clone());
    assert_eq!((card.project, card.version, card.title.as_str()), (project, version, "Home project"));
    assert_eq!(card.badge, crate::cloud_home::Badge::NotUploaded, "no server: it waits in the queue");
    assert!(app.cloud.values().all(|c| c.version == Some(version)));
    assert!(app.pinned.contains(&version), "the open document's revision is pinned");
    assert!(app.recents.ids().contains(&project), "a new project is a recent one");
    // Everything written lives under the adapter directory: the working checkpoint, the queue's cache and the recents.
    let files = listing(&dir);
    assert!(!files.is_empty() && files.iter().all(|f| f.starts_with(&dir)));
    let top: std::collections::BTreeSet<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(top.iter().all(|n| ["working", "sync", "recents.json", "home.json", "account.json"].contains(&n.as_str())), "{top:?}");
    assert!(files.iter().any(|f| f.starts_with(dir.join("working")) && f.extension().is_some_and(|e| e == "pcraft")));
    // Nothing appeared beside the process (no Documents, Desktop or working-directory files).
    let after = std::fs::read_dir(&here).unwrap().flatten().map(|e| e.path()).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(before, after);
    // The checkpoint carried a preview, which decodes to a small picture.
    let preview = card.preview.clone().expect("the queued revision has a preview");
    let decoded = crate::cloud_home::decode_preview(&preview).expect("the preview is a valid small PNG");
    assert!(decoded.size[0].max(decoded.size[1]) <= 256 && decoded.size == [64, 48]);
    app.inner.session.close(0);
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_clean_start_has_no_projects_and_is_not_waiting_for_a_server() {
    let dir = temp("empty");
    let (app, _ctx) = ready(&dir);
    assert!(!app.connected);
    model(&app, |m| {
        assert!(m.ready && m.cards.is_empty() && m.listing_done && !m.connected);
        assert_eq!(m.cache_limit, crate::cloud_home::DEFAULT_CACHE_LIMIT);
    });
    assert_eq!(app.inner.ui.chrome.home, Some(0), "the library is the first screen");
    let mut app = app;
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn closing_a_durable_cloud_document_asks_nothing_and_an_edited_one_is_flushed_first() {
    let dir = temp("close");
    let (mut app, ctx) = ready(&dir);
    let (_, first_version) = new_project(&mut app, &ctx);
    // Durable and untouched, then edited again without waiting for the autosave.
    app.inner.run("layer.new.layer", json!({})).unwrap();
    assert!(app.inner.session.active().unwrap().is_dirty());
    app.tick(&ctx);
    let revision = app.inner.session.active().unwrap().revision;
    let id = app.inner.session.active().unwrap().doc.id;
    assert_ne!(app.tracked[&id].durable, Some(revision), "the edit is not in a checkpoint yet");
    let layers = app.inner.session.active().unwrap().doc.layers.len();
    menus::invoke(&mut app.inner, &ctx, "file.close", json!({})).unwrap();
    assert!(!app.inner.discard_prompt_open(), "no unsaved-changes prompt for a cloud document");
    assert!(app.inner.session.documents().is_empty(), "the tab closed");
    // The checkpoint taken while closing holds the edit.
    app.tick(&ctx);
    pump(&mut app, &ctx, "the edited revision", |a| model(a, |m| m.cards.first().is_some_and(|c| c.version != first_version)));
    let reopened = {
        let card = model(&app, |m| m.cards[0].clone());
        app.perform(&ctx, Action::Open { project: card.project, version: card.version });
        pump(&mut app, &ctx, "the reopened document", |a| !a.inner.session.documents().is_empty());
        app.inner.session.active().unwrap().doc.layers.len()
    };
    assert_eq!(reopened, layers, "the edit made just before closing was kept");
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn files_on_disk_and_documents_not_made_from_home_keep_their_prompt() {
    let dir = temp("prompt");
    let (mut app, ctx) = ready(&dir);
    // A cloud document that was then saved to a file of its own (Save As) keeps the normal prompt.
    new_project(&mut app, &ctx);
    app.inner.session.active_mut().unwrap().path = Some(dir.join("on-disk.pcraft").to_string_lossy().into_owned());
    app.tick(&ctx);
    app.inner.run("layer.new.layer", json!({})).unwrap();
    app.tick(&ctx);
    menus::invoke(&mut app.inner, &ctx, "file.close", json!({})).unwrap();
    assert!(app.inner.discard_prompt_open(), "a file on disk keeps the unsaved-changes prompt");
    assert_eq!(app.inner.session.documents().len(), 1);
    app.inner.session.close(0);
    app.tick(&ctx);
    // A new document made with File › New rather than Home's New is not the cloud's to discard.
    app.inner.run("file.new", json!({"width": 16, "height": 16})).unwrap();
    app.tick(&ctx);
    app.inner.run("layer.new.layer", json!({})).unwrap();
    app.tick(&ctx);
    assert!(app.cloud.values().all(|c| !c.cloud));
    menus::invoke(&mut app.inner, &ctx, "file.close", json!({})).unwrap();
    assert!(app.inner.discard_prompt_open());
    assert_eq!(app.inner.session.documents().len(), 1);
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_close_gate_flushes_only_when_it_must_and_falls_back_to_the_prompt() {
    let (tx, rx) = mpsc::channel();
    let mut gate = CloseGate { docs: HashMap::new(), tx, session_key: "s".into(), wait: Duration::from_millis(300) };
    let mut state = photocraft_engine::DocState::new(
        Document::new("x", photocraft_geom::Size::new(4, 4), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8),
        None,
    );
    state.revision = 5;
    let id = state.doc.id;
    let put = |gate: &mut CloseGate, cloud: bool, durable: Option<u64>| {
        gate.docs.insert(id, GateDoc { key: "k".into(), cloud, durable });
    };
    // Unknown documents and non-cloud documents ask.
    assert!(!gate.allows(&state));
    put(&mut gate, false, Some(5));
    assert!(!gate.allows(&state));
    // Durable cloud document: nothing is sent.
    put(&mut gate, true, Some(5));
    assert!(gate.allows(&state));
    assert!(rx.try_recv().is_err());
    // A file on disk asks, whatever the checkpoint says.
    state.path = Some("/tmp/f.pcraft".into());
    assert!(!gate.allows(&state));
    state.path = None;
    assert!(rx.try_recv().is_err());
    // Not durable: a checkpoint is requested; the answer decides. A responder thread plays the worker.
    let (answers, scripted) = mpsc::channel::<Option<bool>>();
    let responder = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(command) = rx.recv() {
            let Command::Flush { revision, ack, key, source, .. } = command else { panic!("expected a flush") };
            seen.push((revision, key, source));
            if let Ok(Some(ok)) = scripted.recv() {
                let _ = ack.send(ok);
            }
        }
        seen
    });
    put(&mut gate, true, Some(4));
    answers.send(Some(true)).unwrap();
    assert!(gate.allows(&state));
    assert_eq!(gate.docs[&id].durable, Some(5), "remembered, so a second ask sends nothing");
    assert!(gate.allows(&state));
    // The checkpoint failed: the normal prompt.
    put(&mut gate, true, Some(4));
    answers.send(Some(false)).unwrap();
    assert!(!gate.allows(&state));
    assert_eq!(gate.docs[&id].durable, Some(4));
    // No answer in time: the normal prompt.
    answers.send(None).unwrap();
    assert!(!gate.allows(&state));
    drop(gate);
    drop(answers);
    assert_eq!(
        responder.join().unwrap(),
        [(5, "k".to_string(), None), (5, "k".to_string(), None), (5, "k".to_string(), None)],
        "one flush per ask, none for the repeat"
    );
    // The worker is gone: the normal prompt.
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let mut orphan = CloseGate { docs: HashMap::new(), tx, session_key: "s".into(), wait: Duration::from_millis(300) };
    orphan.docs.insert(id, GateDoc { key: "k".into(), cloud: true, durable: Some(1) });
    assert!(!orphan.allows(&state));
}

#[test]
fn opening_a_card_switches_to_the_open_document_or_opens_the_revision_bound_to_its_project() {
    let dir = temp("open");
    let (mut app, ctx) = ready(&dir);
    let (project, version) = new_project(&mut app, &ctx);
    // Already open: switch to it, no second document.
    app.inner.run("file.new", json!({"width": 8, "height": 8})).unwrap();
    assert_eq!(app.inner.session.documents().len(), 2);
    app.inner.ui.chrome.home = Some(2);
    app.perform(&ctx, Action::Open { project, version });
    assert_eq!(app.inner.session.documents().len(), 2);
    assert_eq!(app.inner.session.active_index(), Some(0));
    assert_eq!(app.inner.ui.chrome.home, None, "Home is left");
    // Closed: it opens from the queue, bound, clean and with its content.
    let layers = app.inner.session.documents()[0].doc.layers.len();
    app.inner.session.close(1);
    app.inner.session.close(0);
    app.tick(&ctx);
    assert!(app.inner.session.documents().is_empty());
    app.perform(&ctx, Action::Open { project, version });
    assert_eq!(model(&app, |m| m.busy.get(&version).cloned()), Some(Busy::Opening));
    app.perform(&ctx, Action::Open { project, version });
    pump(&mut app, &ctx, "the document", |a| !a.inner.session.documents().is_empty());
    app.tick(&ctx);
    assert_eq!(app.inner.session.documents().len(), 1, "a second click while it opens does nothing");
    let doc = app.inner.session.active().unwrap();
    assert_eq!((doc.doc.name.as_str(), doc.doc.layers.len(), doc.is_dirty(), doc.path.is_none()), ("Home project", layers, false, true));
    let id = doc.doc.id;
    assert_eq!(app.cloud[&id], CloudDoc { project: Some(project), version: Some(version), cloud: true });
    assert!(app.tracked[&id].key.starts_with("cloud-"));
    assert_eq!(model(&app, |m| m.busy.get(&version).cloned()), None);
    assert!(app.recents.ids().contains(&project));
    // Editing continues the same project.
    app.inner.run("layer.new.layer", json!({})).unwrap();
    settle(&mut app, &ctx);
    assert_eq!(model(&app, |m| m.cards.len()), 1, "still one project, one card");
    assert_eq!(app.cloud[&id].project, Some(project));
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn conflict_cards_never_operate_on_another_open_head() {
    let dir = temp("conflict-card-identity");
    let (mut app, ctx) = ready(&dir);
    app.inner.run("file.new", json!({"width": 8, "height": 8, "name": "Open branch"})).unwrap();
    let branch_id = app.inner.session.active().unwrap().doc.id;
    app.inner.run("file.new", json!({"width": 8, "height": 8, "name": "Unrelated"})).unwrap();
    let project = Uuid::new_v4();
    let open_version = Uuid::new_v4();
    let other_version = Uuid::new_v4();
    let heads = [open_version, other_version];
    app.cloud.insert(branch_id, CloudDoc { project: Some(project), version: Some(open_version), cloud: true });
    app.catalog = vec![
        catalog_head(project, open_version, &heads, "Open branch", dir.join("open.pcraft")),
        catalog_head(project, other_version, &heads, "Other branch", dir.join("other.pcraft")),
    ];
    app.inner.ui.chrome.home = Some(2);

    // Opening the other conflict card starts that revision; it must not select the open branch.
    app.perform(&ctx, Action::Open { project, version: other_version });
    assert_eq!(app.inner.session.active_index(), Some(1));
    assert_eq!(app.inner.ui.chrome.home, Some(2));
    assert!(app.opening.values().any(|version| *version == other_version));

    // Renaming that card publishes a rename of that revision; it must not edit the open branch.
    app.opening.clear();
    app.home.borrow_mut().model.busy.clear();
    app.perform(&ctx, Action::Rename { project, version: other_version, title: "Renamed other branch".into() });
    assert_eq!(app.inner.session.documents()[0].doc.name, "Open branch");
    assert_eq!(app.renaming.get(&other_version).map(String::as_str), Some("Renamed other branch"));

    // Save a Copy likewise opens the requested revision before invoking the editor's file action.
    app.renaming.clear();
    app.home.borrow_mut().model.busy.clear();
    app.perform(&ctx, Action::SaveCopy { project, version: other_version });
    assert_eq!(app.inner.session.active_index(), Some(1));
    assert_eq!(app.copy_after_open.len(), 1);
    assert!(app.opening.values().any(|version| *version == other_version));

    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_open_is_reported_on_the_card_and_can_be_retried() {
    let dir = temp("open-fail");
    let (mut app, ctx) = ready(&dir);
    let (project, version) = (Uuid::new_v4(), Uuid::new_v4());
    app.perform(&ctx, Action::Open { project, version });
    pump(&mut app, &ctx, "the failure", |a| matches!(model(a, |m| m.busy.get(&version).cloned()), Some(Busy::Failed { .. })));
    let Some(Busy::Failed { error, rename }) = model(&app, |m| m.busy.get(&version).cloned()) else { panic!("failed") };
    assert!(error.contains("not in the catalog") && rename.is_none(), "{error}");
    assert!(app.error.is_none(), "the message belongs to the card, not the whole library");
    app.perform(&ctx, Action::Open { project, version });
    assert_eq!(model(&app, |m| m.busy.get(&version).cloned()), Some(Busy::Opening), "Retry tries again");
    pump(&mut app, &ctx, "the second failure", |a| matches!(model(a, |m| m.busy.get(&version).cloned()), Some(Busy::Failed { .. })));
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn renaming_an_open_project_renames_the_document_and_the_next_autosave_carries_it() {
    let dir = temp("rename-open");
    let (mut app, ctx) = ready(&dir);
    let (project, version) = new_project(&mut app, &ctx);
    app.perform(&ctx, Action::Rename { project, version, title: "  Renamed project  ".into() });
    let doc = app.inner.session.active().unwrap();
    assert_eq!(doc.doc.name, "Renamed project");
    assert!(doc.is_dirty(), "a rename is an edit, so autosave notices it");
    app.tick(&ctx);
    assert_eq!(model(&app, |m| m.cards[0].title.clone()), "Renamed project", "the card follows at once");
    due(&mut app);
    pump(&mut app, &ctx, "the renamed revision", |a| a.catalog.iter().any(|r| r.title == "Renamed project"));
    // A bad name changes nothing and is reported on the card.
    app.perform(&ctx, Action::Rename { project, version, title: "   ".into() });
    assert_eq!(app.inner.session.active().unwrap().doc.name, "Renamed project");
    assert!(matches!(model(&app, |m| m.busy.get(&version).cloned()), Some(Busy::Failed { .. })));
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn renaming_a_closed_project_publishes_a_renamed_revision() {
    let dir = temp("rename-closed");
    let (mut app, ctx) = ready(&dir);
    let (project, version) = new_project(&mut app, &ctx);
    app.inner.session.close(0);
    app.tick(&ctx);
    app.perform(&ctx, Action::Rename { project, version, title: "Closed and renamed".into() });
    assert_eq!(model(&app, |m| m.busy.get(&version).cloned()), Some(Busy::Renaming));
    pump(&mut app, &ctx, "the rename", |a| model(a, |m| m.cards.len() == 1 && m.cards[0].title == "Closed and renamed" && m.busy.is_empty()));
    let card = model(&app, |m| m.cards[0].clone());
    assert_eq!(card.project, project);
    assert_ne!(card.version, version, "a rename is a new revision of the same project");
    assert!(app.inner.session.documents().is_empty(), "renaming does not open the project");
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn save_a_copy_of_a_closed_project_opens_it_and_asks_where_to_save() {
    let dir = temp("copy");
    let asked: Rc<RefCell<Vec<photocraft_ui_egui::FileDialogRequest>>> = Rc::default();
    let log = Rc::clone(&asked);
    let services = photocraft_ui_egui::Services {
        file_dialog: Some(Box::new(move |request, _parent, reply| {
            log.borrow_mut().push(request);
            reply.send(None);
        })),
        ..Default::default()
    };
    let mut app = desktop_with(&dir, services);
    let ctx = egui::Context::default();
    pump(&mut app, &ctx, "the worker", |a| a.ready);
    let (project, version) = new_project(&mut app, &ctx);
    app.inner.session.close(0);
    app.tick(&ctx);
    app.perform(&ctx, Action::SaveCopy { project, version });
    pump(&mut app, &ctx, "the Save a Copy dialog", |a| {
        a.inner.poll_file_dialog(&ctx, None);
        !asked.borrow().is_empty()
    });
    assert!(matches!(asked.borrow().first(), Some(photocraft_ui_egui::FileDialogRequest::Save { suggested }) if suggested.contains("Home project")));
    assert_eq!(app.inner.session.documents().len(), 1, "the project is open so the app's own Save a Copy can write it");
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_cache_limit_is_applied_from_the_account_window_and_remembered() {
    let dir = temp("cache");
    let (mut app, ctx) = ready(&dir);
    app.tx.send(Command::Home(HomeCommand::CacheLimit(512 * 1024 * 1024))).unwrap();
    pump(&mut app, &ctx, "the new limit", |a| model(a, |m| m.cache_limit == 512 * 1024 * 1024));
    app.on_exit();
    // A restart starts from the saved choice.
    let (app, _ctx) = ready(&dir);
    assert_eq!(app.cache_limit, 512 * 1024 * 1024);
    assert_eq!(model(&app, |m| m.cache_limit), 512 * 1024 * 1024);
    let mut app = app;
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_control_channel_lists_projects_and_opens_by_version() {
    let dir = temp("control");
    let (mut app, ctx) = ready(&dir);
    let (project, version) = new_project(&mut app, &ctx);
    app.inner.session.close(0);
    app.tick(&ctx);
    let (tx, rx) = mpsc::channel();
    app.control = Some(rx);
    let call = |app: &mut DesktopApp, method: &str, params: Value| {
        let (request, reply) = photocraft_ui_egui::ControlRequest::new(method, params);
        tx.send(request).unwrap();
        app.drain_control(&ctx);
        reply.recv_timeout(Duration::from_secs(5)).unwrap()
    };
    let listed = call(&mut app, "home.list", json!({}));
    assert_eq!(listed["ok"], true);
    assert_eq!(listed["result"]["projects"][0]["version"], version.to_string());
    assert_eq!(listed["result"]["projects"][0]["project"], project.to_string());
    assert_eq!(listed["result"]["projects"][0]["title"], "Home project");
    assert_eq!(listed["result"]["projects"][0]["badge"], "Not uploaded yet");
    assert_eq!(call(&mut app, "home.open", json!({"version": "nope"}))["ok"], false);
    assert_eq!(call(&mut app, "home.open", json!({"version": Uuid::new_v4().to_string()}))["ok"], false);
    assert_eq!(call(&mut app, "home.nothing", json!({}))["ok"], false);
    assert_eq!(call(&mut app, "home.open", json!({"version": version.to_string()}))["ok"], true);
    pump(&mut app, &ctx, "the opened project", |a| !a.inner.session.documents().is_empty());
    assert_eq!(app.cloud.values().next().and_then(|c| c.project), Some(project));
    app.on_exit();
    std::fs::remove_dir_all(dir).unwrap();
}
