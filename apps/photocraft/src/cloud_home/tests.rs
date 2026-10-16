use super::*;
use craft_cloud_sync::{Config, Snapshot, SyncEvent};
use photocraft_doc::{Document, Layer, LayerContent};

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("photocraft-home-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A head revision (or a history revision when `heads` does not contain it).
#[allow(clippy::too_many_arguments)]
fn rev(project: u128, version: u128, title: &str, created: Option<u64>, cached: bool, pending: bool, heads: &[u128]) -> DownloadedRevision {
    DownloadedRevision {
        project_id: id(project),
        version_id: id(version),
        parents: Vec::new(),
        heads: heads.iter().map(|h| id(*h)).collect(),
        title: title.into(),
        extension: "pcraft".into(),
        local_path: PathBuf::from(format!("/cache/{version}")),
        created_ms: created,
        size: 10,
        preview_path: None,
        cached,
        pending,
    }
}

fn card(project: u128, version: u128, title: &str, created: Option<u64>) -> Card {
    Card {
        project: id(project),
        version: id(version),
        title: title.into(),
        created_ms: created,
        cached: false,
        pending: false,
        conflict: None,
        preview: None,
        badge: Badge::InTheCloud,
        open: false,
    }
}

fn titles(cards: &[Card], shown: &[usize]) -> Vec<String> {
    shown.iter().map(|&i| cards[i].title.clone()).collect()
}

fn doc(width: u32, height: u32) -> Document {
    Document::new("Home test", photocraft_geom::Size::new(width, height), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8)
}

#[test]
fn search_is_a_case_insensitive_substring_of_the_title() {
    let cards = vec![card(1, 1, "Summer Poster", Some(3)), card(2, 2, "poster draft", Some(2)), card(3, 3, "Logo", Some(1))];
    let all = |q: &str| titles(&cards, &visible(&cards, q, Filter::All, Sort::LastEdited, &[]));
    assert_eq!(all(""), ["Summer Poster", "poster draft", "Logo"]);
    assert_eq!(all("POSTER"), ["Summer Poster", "poster draft"]);
    assert_eq!(all("  er p "), ["Summer Poster"], "the trimmed query is still a contiguous substring, including the space");
    assert_eq!(all("poster summer"), Vec::<String>::new(), "search is not an unordered word-wise match");
    assert_eq!(all("mer pos"), ["Summer Poster"]);
    assert_eq!(all("nothing"), Vec::<String>::new());
    assert_eq!(all("ÉCOLE"), Vec::<String>::new());
    let unicode = vec![card(1, 1, "École d'été", None)];
    assert_eq!(visible(&unicode, "école", Filter::All, Sort::LastEdited, &[]), [0]);
}

#[test]
fn sorting_by_last_edited_and_by_name() {
    let cards = vec![card(1, 1, "banana", Some(20)), card(2, 2, "Apple", Some(10)), card(3, 3, "cherry", None), card(4, 4, "Banana", Some(30))];
    let order = |sort| titles(&cards, &visible(&cards, "", Filter::All, sort, &[]));
    assert_eq!(order(Sort::LastEdited), ["Banana", "banana", "Apple", "cherry"], "newest first, a revision without a time last");
    assert_eq!(order(Sort::NameAsc), ["Apple", "Banana", "banana", "cherry"], "case-insensitive, ties by exact title");
    assert_eq!(order(Sort::NameDesc), ["cherry", "banana", "Banana", "Apple"]);
    assert_eq!(Sort::default(), Sort::LastEdited);
    assert_eq!(Sort::ALL.map(Sort::label), ["Last edited", "Name A–Z", "Name Z–A"]);
}

#[test]
fn filters_pick_recents_conflicts_offline_and_unsent_projects() {
    let mut cards = vec![card(1, 1, "a", Some(1)), card(2, 2, "b", Some(2)), card(3, 3, "c", Some(3)), card(3, 4, "c2", Some(4)), card(5, 5, "e", Some(5))];
    cards[0].cached = true;
    cards[1].pending = true;
    cards[2].conflict = Some((1, 2));
    cards[3].conflict = Some((2, 2));
    let pick = |filter| titles(&cards, &visible(&cards, "", filter, Sort::NameAsc, &[id(5), id(1)]));
    assert_eq!(pick(Filter::All), ["a", "b", "c", "c2", "e"]);
    assert_eq!(pick(Filter::Recents), ["a", "e"]);
    assert_eq!(pick(Filter::Conflicts), ["c", "c2"]);
    assert_eq!(pick(Filter::Offline), ["a"]);
    assert_eq!(pick(Filter::NotUploaded), ["b"]);
    assert_eq!(titles(&cards, &visible(&cards, "c", Filter::Conflicts, Sort::NameAsc, &[])), ["c", "c2"], "search and filter combine");
    assert_eq!(Filter::ALL.map(Filter::label), ["All", "Recents", "Conflicts", "Available offline", "Not uploaded yet"]);
}

#[test]
fn badges_follow_the_documented_precedence() {
    // (pending, cached, requested, conflict) -> badge
    let cases = [
        ((true, true, false, None), Badge::NotUploaded),
        ((true, true, true, Some((1, 2))), Badge::NotUploaded),
        ((false, false, true, None), Badge::Downloading),
        ((false, false, true, Some((1, 2))), Badge::Downloading),
        ((false, true, true, None), Badge::OnThisComputer),
        ((false, true, false, Some((2, 3))), Badge::Conflict { copy: 2, of: 3 }),
        ((false, false, false, Some((1, 2))), Badge::Conflict { copy: 1, of: 2 }),
        ((false, true, false, None), Badge::OnThisComputer),
        ((false, false, false, None), Badge::InTheCloud),
    ];
    for ((pending, cached, requested, conflict), expected) in cases {
        assert_eq!(badge_for(pending, cached, requested, conflict), expected, "{pending} {cached} {requested} {conflict:?}");
    }
    assert_eq!(Badge::NotUploaded.label(), "Not uploaded yet");
    assert_eq!(Badge::Downloading.label(), "Downloading…");
    assert_eq!(Badge::OnThisComputer.label(), "On this computer");
    assert_eq!(Badge::InTheCloud.label(), "In the cloud");
    assert_eq!(Badge::Conflict { copy: 1, of: 2 }.label(), "Conflict copy 1/2");
}

#[test]
fn the_card_list_has_one_card_per_head_and_numbers_conflict_copies() {
    // Project 1: v10 -> v11 (history) and v12 (head), v13 (head): a conflict between v12 and v13.
    // Project 2: a single head, only in the cloud. Project 3: a pending head that is cached.
    let catalog = vec![
        rev(1, 10, "Old name", Some(100), true, false, &[12, 13]),
        rev(1, 12, "Poster", Some(300), true, false, &[12, 13]),
        rev(1, 13, "Poster (other computer)", Some(200), false, false, &[12, 13]),
        rev(2, 20, "Logo", Some(150), false, false, &[20]),
        rev(3, 30, "Draft", Some(400), true, true, &[30]),
    ];
    let requested: HashSet<Uuid> = [id(20)].into();
    let cards = cards_from_catalog(&catalog, &requested, &HashMap::new());
    assert_eq!(cards.len(), 4, "history revisions get no card");
    assert_eq!(cards.iter().map(|c| c.version).collect::<Vec<_>>(), [id(30), id(12), id(13), id(20)], "newest first by default");
    let by_version = |v: u128| cards.iter().find(|c| c.version == id(v)).unwrap();
    // Conflict copies are numbered oldest first.
    assert_eq!(by_version(13).conflict, Some((1, 2)));
    assert_eq!(by_version(12).conflict, Some((2, 2)));
    assert_eq!(by_version(13).badge, Badge::Conflict { copy: 1, of: 2 });
    assert_eq!(by_version(12).badge, Badge::Conflict { copy: 2, of: 2 });
    assert_eq!(by_version(20).conflict, None);
    assert_eq!(by_version(20).badge, Badge::Downloading, "requested and not cached");
    assert_eq!(by_version(30).badge, Badge::NotUploaded);
    assert!(by_version(30).cached && by_version(30).pending);
    // The live name of an open project replaces the stored one, except across conflict copies.
    let open: HashMap<Uuid, String> = [(id(3), "Renamed draft".to_string()), (id(1), "Ignored".to_string())].into();
    let cards = cards_from_catalog(&catalog, &HashSet::new(), &open);
    let by_version = |v: u128| cards.iter().find(|c| c.version == id(v)).unwrap();
    assert_eq!(by_version(30).title, "Renamed draft");
    assert!(by_version(30).open && by_version(12).open && !by_version(20).open);
    assert_eq!(by_version(12).title, "Poster");
    // An empty catalog is an empty list.
    assert!(cards_from_catalog(&[], &HashSet::new(), &HashMap::new()).is_empty());
}

#[test]
fn the_recents_row_lists_the_newest_card_of_each_recent_project() {
    let mut cards: Vec<Card> = (1..=12).map(|n| card(n, n + 100, &format!("p{n}"), Some(n as u64))).collect();
    cards.push(card(2, 202, "p2 newer", Some(50)));
    let recents: Vec<Uuid> = (1..=12).rev().map(id).collect();
    let row = recent_cards(&cards, &recents);
    assert_eq!(row.len(), RECENTS_MAX);
    assert_eq!(cards[row[0]].title, "p12");
    let row = recent_cards(&cards, &[id(2), id(99), id(1)]);
    assert_eq!(row.iter().map(|&i| cards[i].title.as_str()).collect::<Vec<_>>(), ["p2 newer", "p1"], "unknown projects are skipped, the newest head wins");
}

#[test]
fn recents_keep_the_eight_newest_and_survive_a_restart() {
    let dir = temp("recents");
    let path = recents_path(&dir);
    assert_eq!(Recents::load(&path), Recents::default(), "no file yet");
    let mut recents = Recents::default();
    for n in 1..=10u128 {
        recents.touch(id(n), n as u64 * 10);
    }
    recents.touch(id(5), 500);
    assert_eq!(recents.ids().len(), RECENTS_MAX);
    assert_eq!(recents.ids()[0], id(5), "opening a project again moves it to the front");
    assert!(!recents.ids().contains(&id(1)) && !recents.ids().contains(&id(2)));
    assert_eq!(recents.ids().iter().filter(|p| **p == id(5)).count(), 1);
    recents.save(&path).unwrap();
    assert_eq!(Recents::load(&path), recents);
    assert!(std::fs::metadata(&path).unwrap().len() < RECENTS_FILE_MAX);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_corrupt_or_oversized_recents_file_is_an_empty_list() {
    let dir = temp("recents-bad");
    let path = recents_path(&dir);
    for bad in [&b"not json at all"[..], b"", b"[1,2,3]", b"{\"projects\": 7}", b"{\"projects\": [", &[0xff, 0xfe, 0x00][..]] {
        std::fs::write(&path, bad).unwrap();
        assert_eq!(Recents::load(&path), Recents::default(), "{:?}", String::from_utf8_lossy(bad));
    }
    // Bad entries are skipped, good ones kept, duplicates collapse.
    let mixed = format!(
        "{{\"projects\":[{{\"project\":\"{}\",\"opened_ms\":5}},{{\"project\":\"not-a-uuid\"}},{{\"opened_ms\":1}},7,{{\"project\":\"{}\",\"opened_ms\":9}},{{\"project\":\"{}\",\"opened_ms\":1}}]}}",
        id(1),
        id(2),
        id(1)
    );
    std::fs::write(&path, mixed).unwrap();
    assert_eq!(Recents::load(&path).ids(), [id(2), id(1)]);
    // Larger than the limit: ignored, however valid.
    let projects: Vec<_> = (0..1000u128).map(|n| format!("{{\"project\":\"{}\",\"opened_ms\":{n}}}", id(n))).collect();
    let huge = format!("{{\"projects\":[{}]}}", projects.join(","));
    assert!(huge.len() as u64 > RECENTS_FILE_MAX);
    std::fs::write(&path, huge).unwrap();
    assert_eq!(Recents::load(&path), Recents::default());
    // A file with many valid entries within the limit is cut to eight.
    let projects: Vec<_> = (0..40u128).map(|n| format!("{{\"project\":\"{}\",\"opened_ms\":{n}}}", id(n))).collect();
    std::fs::write(&path, format!("{{\"projects\":[{}]}}", projects.join(","))).unwrap();
    let loaded = Recents::load(&path);
    assert_eq!(loaded.ids().len(), RECENTS_MAX);
    assert_eq!(loaded.ids()[0], id(39));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rename_validation_trims_and_rejects_empty_long_and_control_characters() {
    assert_eq!(validate_title("  My poster  "), Ok("My poster".into()));
    assert_eq!(validate_title("日本語の名前"), Ok("日本語の名前".into()));
    assert!(validate_title("").is_err());
    assert!(validate_title("   \t ").is_err());
    assert!(validate_title("two\nlines").is_err());
    assert!(validate_title("bell\u{7}").is_err());
    assert!(validate_title("nul\0").is_err());
    assert_eq!(validate_title(&"x".repeat(256)).map(|t| t.len()), Ok(256));
    assert!(validate_title(&"x".repeat(257)).is_err());
    // The shared client counts bytes: 129 two-byte characters would be refused later, so refuse them now.
    assert!(validate_title(&"é".repeat(129)).is_err());
    assert!(validate_title(&"é".repeat(128)).is_ok());
}

#[test]
fn edited_times_are_relative_and_old_ones_are_dates() {
    let now = 1_700_000_000_000u64; // 2023-11-14 22:13:20 UTC
    let ago = |secs: u64| edited_label(Some(now - secs * 1000), now);
    assert_eq!(edited_label(None, now), "Edited —");
    assert_eq!(ago(5), "Edited just now");
    assert_eq!(edited_label(Some(now + 5000), now), "Edited just now", "a clock a little ahead is not the future");
    assert_eq!(ago(60), "Edited 1 minute ago");
    assert_eq!(ago(5 * 60), "Edited 5 minutes ago");
    assert_eq!(ago(3600), "Edited 1 hour ago");
    assert_eq!(ago(3 * 3600), "Edited 3 hours ago");
    assert_eq!(ago(86_400), "Edited yesterday");
    assert_eq!(ago(3 * 86_400), "Edited 3 days ago");
    assert_eq!(ago(40 * 86_400), "Edited Oct 5, 2023");
    assert_eq!(edited_label(Some(0), now), "Edited Jan 1, 1970");
    assert_eq!(edited_label(Some(951_782_400_000), now), "Edited Feb 29, 2000");
    assert_eq!(edited_label(Some(u64::MAX), 0), "Edited just now");
}

#[test]
fn sizes_read_naturally() {
    assert_eq!(bytes_label(0), "0 KB");
    assert_eq!(bytes_label(1500), "2 KB");
    assert_eq!(bytes_label(5 * MIB + MIB / 2), "5.5 MB");
    assert_eq!(cache_label(120 * MIB, DEFAULT_CACHE_LIMIT), "Cache: 120.0 MB of 2.0 GB");
}

#[test]
fn the_cache_limit_setting_is_a_preset_with_a_two_gigabyte_default() {
    let dir = temp("limit");
    let path = settings_path(&dir);
    assert_eq!(load_cache_limit(&path), DEFAULT_CACHE_LIMIT);
    assert_eq!(DEFAULT_CACHE_LIMIT, 2 * 1024 * MIB);
    for preset in CACHE_PRESETS {
        save_cache_limit(&path, preset).unwrap();
        assert_eq!(load_cache_limit(&path), preset);
    }
    assert_eq!(CACHE_PRESETS, [512 * MIB, 1024 * MIB, 2048 * MIB, 5120 * MIB, 10_240 * MIB]);
    for bad in ["{\"cache_limit\": 12345}", "{\"cache_limit\": \"big\"}", "garbage", "{}", ""] {
        std::fs::write(&path, bad).unwrap();
        assert_eq!(load_cache_limit(&path), DEFAULT_CACHE_LIMIT, "{bad}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_pinned_set_holds_the_revisions_of_open_documents() {
    let docs = [
        CloudDoc { project: Some(id(1)), version: Some(id(11)), cloud: true },
        CloudDoc { project: None, version: None, cloud: true },
        CloudDoc { project: Some(id(2)), version: Some(id(22)), cloud: false },
        CloudDoc { project: Some(id(3)), version: Some(id(11)), cloud: true },
    ];
    assert_eq!(pinned_set(&docs), BTreeSet::from([id(11), id(22)]));
    assert!(pinned_set(&[]).is_empty());
}

#[test]
fn closing_skips_the_prompt_only_for_durable_cloud_documents_without_a_file() {
    use CloseDecision::*;
    // (has_path, cloud, durable, revision)
    assert_eq!(close_decision(false, true, Some(7), 7), Safe);
    assert_eq!(close_decision(false, true, Some(6), 7), Flush, "edited since the checkpoint");
    assert_eq!(close_decision(false, true, None, 7), Flush, "never checkpointed");
    assert_eq!(close_decision(true, true, Some(7), 7), Prompt, "a file on disk keeps its prompt");
    assert_eq!(close_decision(true, true, None, 7), Prompt);
    assert_eq!(close_decision(false, false, Some(7), 7), Prompt, "documents the cloud does not own keep their prompt");
    assert_eq!(close_decision(false, false, None, 7), Prompt);
}

#[test]
fn a_preview_is_a_small_png_of_the_flattened_document() {
    for (w, h) in [(3000, 1000), (1000, 3000), (200, 100), (1, 1), (5000, 1)] {
        let png = preview_png(&doc(w, h)).unwrap_or_else(|| panic!("no preview for {w}x{h}"));
        assert!(png.starts_with(PNG_SIGNATURE) && png.len() <= PREVIEW_MAX_BYTES, "{w}x{h}");
        let (pw, ph) = png_dimensions(&png).unwrap();
        assert!(pw.max(ph) <= PREVIEW_SIDE, "{w}x{h} -> {pw}x{ph}");
        assert!(pw >= 1 && ph >= 1);
        // Same proportions, within a pixel of rounding.
        assert!(
            (f64::from(pw) / f64::from(ph) - f64::from(w) / f64::from(h)).abs()
                <= f64::from(w.max(h)) / f64::from(w.min(h)) / f64::from(pw.min(ph)).max(1.0) + 1e-6
        );
    }
    // A document with content decodes back to the same size.
    let mut painted = doc(640, 480);
    painted.layers.push(Layer::new(
        "Fill",
        LayerContent::Raster(photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(
            photocraft_color::ColorMode::Rgb,
            photocraft_color::SampleType::U8,
            true,
        ))),
    ));
    let png = preview_png(&painted).unwrap();
    let dir = temp("preview");
    let path = dir.join("p.png");
    std::fs::write(&path, &png).unwrap();
    let decoded = decode_preview(&path).unwrap();
    assert_eq!(decoded.size, [256, 192]);
    assert_eq!(decoded.rgba.len(), 256 * 192 * 4);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_document_that_cannot_be_rendered_gets_no_preview() {
    assert_eq!(preview_png(&doc(0, 0)), None);
    assert_eq!(preview_png(&doc(0, 100)), None);
}

#[test]
fn a_preview_that_overruns_its_budget_or_overlaps_another_is_skipped() {
    let busy = Arc::new(AtomicBool::new(false));
    let fast = preview_png_bounded(Arc::new(doc(400, 300)), Duration::from_secs(30), &busy);
    assert!(fast.is_some_and(|png| png.starts_with(PNG_SIGNATURE)));
    // The gate is released once the render ends.
    let mut released = false;
    for _ in 0..200 {
        if !busy.load(Ordering::Acquire) {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(released);
    // While another render holds the gate there is no preview, and the gate is not stolen.
    busy.store(true, Ordering::Release);
    assert_eq!(preview_png_bounded(Arc::new(doc(400, 300)), Duration::from_secs(30), &busy), None);
    assert!(busy.load(Ordering::Acquire));
    // A render that cannot finish within the budget gives up (a zero budget cannot be met).
    busy.store(false, Ordering::Release);
    assert_eq!(preview_png_bounded(Arc::new(doc(6000, 6000)), Duration::ZERO, &busy), None);
}

#[test]
fn bad_preview_files_are_placeholders_not_failures() {
    let dir = temp("bad-previews");
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };
    assert_eq!(decode_preview(&dir.join("missing.png")), None);
    assert_eq!(decode_preview(&write("text.png", b"hello")), None);
    assert_eq!(decode_preview(&write("short.png", PNG_SIGNATURE)), None);
    // A header that claims a gigantic picture is refused without decoding.
    let mut header = PNG_SIGNATURE.to_vec();
    header.extend_from_slice(&13u32.to_be_bytes());
    header.extend_from_slice(b"IHDR");
    header.extend_from_slice(&40_000u32.to_be_bytes());
    header.extend_from_slice(&40_000u32.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(decode_preview(&write("huge.png", &header)), None);
    // A truncated real PNG does not decode.
    let good = preview_png(&doc(100, 100)).unwrap();
    assert_eq!(decode_preview(&write("cut.png", &good[..good.len() / 2])), None);
    // Larger than any preview may be.
    let mut big = good.clone();
    big.resize(PREVIEW_MAX_BYTES + 1, 0);
    assert_eq!(decode_preview(&write("big.png", &big)), None);
    assert!(decode_preview(&write("good.png", &good)).is_some());
    std::fs::remove_dir_all(dir).unwrap();
}

fn queue(dir: &Path) -> SyncService {
    SyncService::new(Config { endpoint: String::new(), username: String::new(), password: String::new() }, dir.join("sync"), "photocraft").unwrap()
}

fn snapshot(key: &str, title: &str, token: &str) -> Snapshot {
    Snapshot {
        local_key: key.into(),
        title: title.into(),
        extension: "pcraft".into(),
        local_token: token.into(),
        native: b"project bytes".to_vec(),
        preview: None,
    }
}

fn fetched(service: &SyncService, version: Uuid) -> bool {
    service.poll().events.iter().any(|e| matches!(e, SyncEvent::Fetched { version_id, .. } if *version_id == version))
}

#[test]
fn an_open_waits_for_its_payload_and_then_hands_over_the_project() {
    let dir = temp("open");
    let service = queue(&dir);
    let sent = service.submit(snapshot("k", "Poster", "t1")).unwrap();
    let (mut home, startup) = HomeWorker::new(&dir);
    assert!(startup.recents.is_empty());
    assert_eq!(startup.cache_limit, DEFAULT_CACHE_LIMIT);
    let none = home.handle(HomeCommand::OpenVersion { project: sent.project_id, version: sent.version_id, key: "cloud-1".into() }, Some(&service));
    assert!(none.is_empty());
    assert_eq!(home.waiting(), 1);
    assert!(fetched(&service, sent.version_id), "a cached payload is answered at once");
    let (opens, replies) = home.fetched(sent.version_id, Some(&service));
    assert!(replies.is_empty());
    assert_eq!((opens.len(), home.waiting()), (1, 0));
    assert_eq!((opens[0].key.as_str(), opens[0].project, opens[0].version), ("cloud-1", sent.project_id, sent.version_id));
    // Nothing else is waiting for that version.
    assert!(home.fetched(sent.version_id, Some(&service)).0.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_download_fails_the_waiting_open_and_rename() {
    let dir = temp("open-fail");
    let service = queue(&dir);
    let (mut home, _) = HomeWorker::new(&dir);
    let ghost = id(404);
    home.handle(HomeCommand::OpenVersion { project: id(1), version: ghost, key: "cloud-x".into() }, Some(&service));
    home.handle(HomeCommand::Rename { project: id(1), version: ghost, title: "New".into() }, Some(&service));
    assert_eq!(home.waiting(), 2);
    let failed = service.poll().events.into_iter().find_map(|e| match e {
        SyncEvent::FetchFailed { version_id, error } => Some((version_id, error)),
        _ => None,
    });
    let (version, error) = failed.expect("an unknown revision reports a failed fetch");
    assert_eq!(version, ghost);
    let replies = home.fetch_failed(version, &error);
    assert_eq!(replies.len(), 2);
    assert!(replies.iter().all(|r| matches!(r, HomeReply::Failed { version, error } if *version == ghost && error.contains("not in the catalog"))));
    assert_eq!(home.waiting(), 0);
    // Nothing to open or rename without a sync queue.
    let no_queue = home.handle(HomeCommand::OpenVersion { project: id(1), version: ghost, key: "k".into() }, None);
    assert!(matches!(no_queue.as_slice(), [HomeReply::Failed { .. }]));
    assert_eq!(home.waiting(), 0);
    // A bad name never reaches the queue.
    let bad = home.handle(HomeCommand::Rename { project: id(1), version: ghost, title: "  ".into() }, Some(&service));
    assert!(matches!(bad.as_slice(), [HomeReply::Failed { error, .. }] if error.contains("empty")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reconnecting_drops_what_was_waiting_and_says_so() {
    let dir = temp("interrupt");
    let service = queue(&dir);
    let (mut home, _) = HomeWorker::new(&dir);
    home.handle(HomeCommand::OpenVersion { project: id(1), version: id(2), key: "a".into() }, Some(&service));
    home.handle(HomeCommand::Rename { project: id(1), version: id(3), title: "New".into() }, Some(&service));
    let replies = home.interrupt();
    assert_eq!(replies.len(), 2);
    assert_eq!(home.waiting(), 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn renaming_a_closed_project_publishes_a_renamed_revision() {
    let dir = temp("rename");
    let service = queue(&dir);
    let sent = service.submit(snapshot("k", "Old name", "t1")).unwrap();
    let (mut home, _) = HomeWorker::new(&dir);
    home.handle(HomeCommand::Rename { project: sent.project_id, version: sent.version_id, title: "  New name  ".into() }, Some(&service));
    assert!(fetched(&service, sent.version_id));
    let (opens, replies) = home.fetched(sent.version_id, Some(&service));
    assert!(opens.is_empty());
    let new_version = match replies.as_slice() {
        [HomeReply::Renamed { version, new_version }] => {
            assert_eq!(*version, sent.version_id);
            *new_version
        }
        _ => panic!("expected one Renamed reply"),
    };
    let heads = service.catalog_heads();
    assert_eq!(heads.len(), 1);
    assert_eq!((heads[0].version_id, heads[0].title.as_str(), heads[0].project_id), (new_version, "New name", sent.project_id));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pinning_and_the_cache_limit_reach_the_client_and_survive_a_restart() {
    let dir = temp("pins");
    let service = queue(&dir);
    let (mut home, _) = HomeWorker::new(&dir);
    assert!(home.handle(HomeCommand::CacheLimit(512 * MIB), Some(&service)).is_empty());
    assert!(home.handle(HomeCommand::Pin(BTreeSet::from([id(1)])), Some(&service)).is_empty());
    assert_eq!(service.poll().cache_limit, 512 * MIB);
    // A new queue (after reconnecting) starts with the defaults until the settings are applied again.
    drop(service);
    let service = queue(&dir);
    assert_eq!(service.poll().cache_limit, DEFAULT_CACHE_LIMIT);
    home.apply(&service);
    assert_eq!(service.poll().cache_limit, 512 * MIB);
    let (_, startup) = HomeWorker::new(&dir);
    assert_eq!(startup.cache_limit, 512 * MIB, "persisted in the adapter's settings");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recents_are_written_by_the_worker_and_read_back_at_startup() {
    let dir = temp("recents-worker");
    let (mut home, _) = HomeWorker::new(&dir);
    home.handle(HomeCommand::Recents(vec![(id(3), 30), (id(2), 20)]), None);
    let (_, startup) = HomeWorker::new(&dir);
    assert_eq!(startup.recents, [(id(3), 30), (id(2), 20)]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn previews_are_decoded_in_bounded_batches() {
    let dir = temp("batch");
    let path = dir.join("p.png");
    std::fs::write(&path, preview_png(&doc(300, 200)).unwrap()).unwrap();
    let (mut home, _) = HomeWorker::new(&dir);
    let wanted: Vec<_> = (0..40u128).map(|n| (id(n), if n % 2 == 0 { path.clone() } else { dir.join("nope.png") })).collect();
    let replies = home.handle(HomeCommand::Previews(wanted), None);
    let [HomeReply::Previews(done)] = replies.as_slice() else { panic!("one batch reply") };
    assert_eq!(done.len(), PREVIEW_BATCH);
    assert!(done.iter().all(|(v, d)| d.is_some() == (v.as_u128() % 2 == 0)));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_list_for_agents_names_each_card() {
    let mut c = card(1, 2, "Poster", Some(1000));
    c.badge = Badge::Conflict { copy: 1, of: 2 };
    c.conflict = Some((1, 2));
    let json = list_json(&[c], true, false, "offline", 1000);
    assert_eq!(json["ready"], true);
    assert_eq!(json["connected"], false);
    let project = &json["projects"][0];
    assert_eq!(project["title"], "Poster");
    assert_eq!(project["version"], id(2).to_string());
    assert_eq!(project["badge"], "Conflict copy 1/2");
    assert_eq!(project["conflict"]["of"], 2);
    assert_eq!(project["edited"], "Edited just now");
}

#[test]
fn control_requests_are_split_between_the_editor_and_the_library() {
    let (tx, rx) = mpsc::channel();
    let (editor, home) = split_control(rx, egui::Context::default());
    for method in ["engine.execute", "home.list", "ui.screenshot", "home.open"] {
        tx.send(photocraft_ui_egui::ControlRequest::new(method, serde_json::Value::Null).0).unwrap();
    }
    let wait = Duration::from_secs(5);
    assert_eq!(editor.recv_timeout(wait).unwrap().method, "engine.execute");
    assert_eq!(editor.recv_timeout(wait).unwrap().method, "ui.screenshot");
    assert_eq!(home.recv_timeout(wait).unwrap().method, "home.list");
    assert_eq!(home.recv_timeout(wait).unwrap().method, "home.open");
    drop(tx);
}

/// Writes a working copy the way `checkpoint` does and returns its payload file name.
fn working_copy(working: &Path, key: &str, digit: char, token: &str, source: Option<&str>, needs_submit: bool, portable: bool) -> String {
    let payload = format!("{key}-{}.pcraft", digit.to_string().repeat(64));
    std::fs::write(working.join(&payload), b"native bytes").unwrap();
    let meta = serde_json::json!({"key": key, "title": "t", "source": source, "payload": payload, "sha256": "x", "local_token": token, "portable": portable, "needs_submit": needs_submit});
    std::fs::write(working.join(format!("{key}.json")), serde_json::to_vec(&meta).unwrap()).unwrap();
    payload
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn a_published_closed_working_copy_is_removed_and_nothing_else() {
    let dir = temp("cleanup");
    let open: HashSet<String> = ["open-doc".to_string()].into();
    let gone = working_copy(&dir, "cloud-a", 'a', "tok-a", None, false, true);
    // Older checkpoints of the same key that a newer one replaced.
    std::fs::write(dir.join(format!("cloud-a-{}.pcraft", "1".repeat(64))), b"old").unwrap();
    let other = working_copy(&dir, "cloud-b", 'b', "tok-b", None, false, true);
    assert_eq!(cleanup_published(&dir, "tok-a", &open), Some("cloud-a".to_string()));
    assert_eq!(names(&dir), [other.clone(), "cloud-b.json".to_string()], "only that key's files go, including the superseded one: {gone}");
    // Reporting it again, or a token nobody has, removes nothing.
    assert_eq!(cleanup_published(&dir, "tok-a", &open), None);
    assert_eq!(cleanup_published(&dir, "unknown", &open), None);
    assert_eq!(names(&dir).len(), 2);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_working_copy_is_kept_while_it_is_open_newer_unsent_or_a_files_binding() {
    let dir = temp("cleanup-keep");
    let none = HashSet::new();
    // Open in the editor.
    working_copy(&dir, "open-doc", 'a', "t1", None, false, true);
    assert_eq!(cleanup_published(&dir, "t1", &["open-doc".to_string()].into()), None);
    // A newer checkpoint replaced the metadata: the old token no longer matches and nothing is removed.
    working_copy(&dir, "newer", 'b', "t2-new", None, false, true);
    assert_eq!(cleanup_published(&dir, "t2-old", &none), None);
    // Not handed to the queue yet, or blocked by a missing original.
    working_copy(&dir, "unsent", 'c', "t3", None, true, true);
    assert_eq!(cleanup_published(&dir, "t3", &none), None);
    working_copy(&dir, "blocked", 'd', "t4", None, false, false);
    assert_eq!(cleanup_published(&dir, "t4", &none), None);
    // A document with a file keeps the mapping from its path to its cloud project.
    working_copy(&dir, "file-0123", 'e', "t5", Some("/home/me/pic.pcraft"), false, true);
    assert_eq!(cleanup_published(&dir, "t5", &none), None);
    assert_eq!(names(&dir).len(), 10, "nothing was removed");
    // Metadata that points outside the working directory is never followed.
    let meta =
        serde_json::json!({"key": "evil", "payload": "../../victim.pcraft", "local_token": "t6", "portable": true, "needs_submit": false, "source": null});
    std::fs::write(dir.join("evil.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    assert_eq!(cleanup_published(&dir, "t6", &none), None);
    assert!(dir.join("evil.json").exists());
    // A metadata file for another key than its name says is left alone as well.
    let meta = serde_json::json!({"key": "liar", "payload": format!("liar-{}.pcraft", "f".repeat(64)), "local_token": "t7", "portable": true, "needs_submit": false, "source": null});
    std::fs::write(dir.join("other-name.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    assert_eq!(cleanup_published(&dir, "t7", &none), None);
    assert!(cleanup_published(&dir.join("missing"), "t7", &none).is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn superseded_checkpoints_are_pruned_by_exact_name_only() {
    let dir = temp("prune");
    let keep = format!("k-{}.pcraft", "a".repeat(64));
    for name in [
        keep.clone(),
        format!("k-{}.pcraft", "b".repeat(64)),
        format!("k-x-{}.pcraft", "c".repeat(64)),
        format!("k-{}.pcraft.tmp", "d".repeat(64)),
        "k-short.pcraft".to_string(),
        "k.json".to_string(),
        format!("other-{}.pcraft", "e".repeat(64)),
    ] {
        std::fs::write(dir.join(name), b"x").unwrap();
    }
    prune_superseded(&dir, "k", &keep);
    let left = names(&dir);
    assert_eq!(left.len(), 6, "{left:?}");
    assert!(left.contains(&keep) && !left.iter().any(|n| n.starts_with(&format!("k-{}", "b".repeat(64)))));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_worker_cleans_up_when_told_a_checkpoint_was_published() {
    let dir = temp("worker-cleanup");
    std::fs::create_dir_all(dir.join("working")).unwrap();
    working_copy(&dir.join("working"), "cloud-z", 'a', "tok", None, false, true);
    let (mut home, _) = HomeWorker::new(&dir);
    home.handle(HomeCommand::OpenKeys(["cloud-z".to_string()].into()), None);
    assert_eq!(home.published("tok"), None, "open documents keep their checkpoint");
    home.handle(HomeCommand::OpenKeys(HashSet::new()), None);
    assert_eq!(home.published("tok"), Some("cloud-z".to_string()));
    assert!(names(&dir.join("working")).is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}
