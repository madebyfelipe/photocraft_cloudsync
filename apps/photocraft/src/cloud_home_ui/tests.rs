//! The Home view run through egui frames: every state draws in every theme, clicks and keys become
//! actions, previews become bounded textures.
use super::*;
use crate::cloud_home::Decoded;
use egui::{Event, Modifiers, PointerButton, RawInput, Rect, pos2};
use photocraft_ui_egui::PhotocraftApp;
use photocraft_ui_egui::theme::ThemeKind;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn context(kind: ThemeKind) -> egui::Context {
    let ctx = egui::Context::default();
    PhotocraftApp::setup_context(&ctx, kind);
    // Fonts installed by the setup take effect from the next frame.
    ctx.run_ui(RawInput::default(), |_| {}).drop_without_applying_deltas();
    ctx
}

fn frame(ctx: &egui::Context, home: &mut Home, events: Vec<Event>) {
    let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))), events, ..Default::default() };
    ctx.run_ui(input, |ui| home.show(ui)).drop_without_applying_deltas();
}

fn card(n: u128, title: &str, badge: Badge) -> Card {
    Card {
        project: id(n),
        version: id(n + 1000),
        title: title.into(),
        created_ms: Some(1_700_000_000_000 - n as u64 * 3_600_000),
        cached: matches!(badge, Badge::OnThisComputer),
        pending: matches!(badge, Badge::NotUploaded),
        conflict: if let Badge::Conflict { copy, of } = badge { Some((copy, of)) } else { None },
        preview: None,
        badge,
        open: n == 2,
    }
}

fn ready_model(cards: Vec<Card>) -> Model {
    Model {
        ready: true,
        connected: true,
        listing_done: true,
        cards,
        cache_limit: 2 << 30,
        cache_bytes: 300 << 20,
        now_ms: 1_700_000_000_000,
        ..Default::default()
    }
}

fn five_cards() -> Vec<Card> {
    vec![
        card(1, "Summer poster with a rather long title that cannot fit on one line", Badge::OnThisComputer),
        card(2, "Logo", Badge::InTheCloud),
        card(3, "Draft", Badge::NotUploaded),
        card(4, "Copy a", Badge::Conflict { copy: 1, of: 2 }),
        card(5, "Copy b", Badge::Downloading),
    ]
}

fn press(at: egui::Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

fn key(key: Key, pressed: bool) -> Event {
    Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE }
}

/// Moves to `at`, presses and releases, a frame each.
fn click(ctx: &egui::Context, home: &mut Home, at: egui::Pos2) {
    frame(ctx, home, vec![Event::PointerMoved(at)]);
    frame(ctx, home, vec![press(at, true)]);
    frame(ctx, home, vec![press(at, false)]);
}

fn rect_of(home: &Home, version: Uuid) -> Rect {
    home.rects.iter().find(|(v, _)| *v == version).map(|(_, r)| *r).unwrap_or_else(|| panic!("card {version} was not drawn"))
}

fn button(home: &Home, label: &str) -> Rect {
    home.buttons
        .iter()
        .find(|(l, _)| l == label)
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("no button {label}: {:?}", home.buttons.iter().map(|(l, _)| l).collect::<Vec<_>>()))
}

#[test]
fn every_state_draws_in_every_theme() {
    for kind in ThemeKind::ALL {
        let ctx = context(kind);
        let mut home = Home::default();
        let states: Vec<(&str, Model)> = vec![
            ("starting", Model { ready: false, ..Default::default() }),
            ("loading", Model { listing_done: false, ..ready_model(Vec::new()) }),
            ("empty", ready_model(Vec::new())),
            ("empty and not connected", Model { connected: false, ..ready_model(Vec::new()) }),
            ("cards", ready_model(five_cards())),
            ("cards while listing", Model { listing_done: false, ..ready_model(five_cards()) }),
            ("offline", Model { net: Net::Offline, ..ready_model(five_cards()) }),
            ("not connected", Model { connected: false, ..ready_model(five_cards()) }),
            ("error", Model { error: Some("The server said no".into()), documents_open: true, ..ready_model(five_cards()) }),
            ("sync error", Model { net: Net::Error("boom".into()), ..ready_model(five_cards()) }),
            (
                "busy",
                Model {
                    busy: HashMap::from([
                        (id(1001), Busy::Opening),
                        (id(1002), Busy::Renaming),
                        (
                            id(1003),
                            Busy::Failed { error: "Cannot download this project: offline: the revision payload is not downloaded".into(), rename: None },
                        ),
                    ]),
                    recents: vec![id(1), id(3)],
                    ..ready_model(five_cards())
                },
            ),
        ];
        for (name, model) in states {
            home.model = model;
            for _ in 0..3 {
                frame(&ctx, &mut home, Vec::new());
            }
            if home.model.cards.len() == 5 && home.model.ready {
                assert_eq!(home.rects.len(), 5, "{kind:?} {name}: every card is drawn at 1200x800");
            }
        }
        // A tiny window still lays out.
        let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(180.0, 120.0))), ..Default::default() };
        home.model = ready_model(five_cards());
        ctx.run_ui(input, |ui| home.show(ui)).drop_without_applying_deltas();
    }
}

#[test]
fn clicking_a_card_asks_to_open_it_and_the_header_buttons_ask_for_their_actions() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: Model { documents_open: true, ..ready_model(five_cards()) }, ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    frame(&ctx, &mut home, Vec::new());
    let at = rect_of(&home, id(1002)).center();
    click(&ctx, &mut home, at);
    assert_eq!(home.take_actions(), [Action::Open { project: id(2), version: id(1002) }]);
    for (label, action) in [("New", Action::New), ("Account", Action::Account), ("Back to editor", Action::Back)] {
        let at = button(&home, label).center();
        click(&ctx, &mut home, at);
        assert_eq!(home.take_actions(), [action], "{label}");
    }
    // "Back to editor" only exists when there is an editor to go back to.
    home.model.documents_open = false;
    frame(&ctx, &mut home, Vec::new());
    assert!(!home.buttons.iter().any(|(l, _)| l == "Back to editor"));
}

#[test]
fn the_search_field_sort_and_filter_chips_narrow_the_grid() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: ready_model(five_cards()), ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.rects.len(), 5);
    home.search = "LOGO".into();
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.rects.iter().map(|(v, _)| *v).collect::<Vec<_>>(), [id(1002)]);
    home.search = "zzz".into();
    frame(&ctx, &mut home, Vec::new());
    assert!(home.rects.is_empty());
    home.search.clear();
    let at = button(&home, "Conflicts").center();
    click(&ctx, &mut home, at);
    assert_eq!(home.filter, Filter::Conflicts);
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.rects.iter().map(|(v, _)| *v).collect::<Vec<_>>(), [id(1004)]);
    let at = button(&home, "All").center();
    click(&ctx, &mut home, at);
    assert_eq!(home.filter, Filter::All);
    home.sort = Sort::NameDesc;
    frame(&ctx, &mut home, Vec::new());
    let first = home.rects.iter().min_by(|a, b| a.1.min.y.total_cmp(&b.1.min.y).then(a.1.min.x.total_cmp(&b.1.min.x))).map(|(v, _)| *v);
    assert_eq!(first, Some(id(1001)), "Z–A starts with 'Summer poster…'");
}

#[test]
fn recents_show_above_the_grid_only_without_search_or_filter() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut model = ready_model(five_cards());
    model.recents = vec![id(3), id(1)];
    let mut home = Home { model, ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.recent_rects.iter().map(|(v, _)| *v).collect::<Vec<_>>(), [id(1003), id(1001)], "most recent first");
    home.search = "logo".into();
    frame(&ctx, &mut home, Vec::new());
    assert!(home.recent_rects.is_empty());
    home.search.clear();
    home.filter = Filter::Offline;
    frame(&ctx, &mut home, Vec::new());
    assert!(home.recent_rects.is_empty());
    // A recent tile opens its project, and offers the same menu as a card.
    home.filter = Filter::All;
    frame(&ctx, &mut home, Vec::new());
    let at = home.recent_rects[0].1.center();
    click(&ctx, &mut home, at);
    assert_eq!(home.take_actions(), [Action::Open { project: id(3), version: id(1003) }]);
}

#[test]
fn f2_renames_in_place_with_validation_and_escape_cancels() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: ready_model(five_cards()), ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    let at = rect_of(&home, id(1002)).center();
    frame(&ctx, &mut home, vec![Event::PointerMoved(at)]);
    home.keys.rename = true;
    for _ in 0..4 {
        frame(&ctx, &mut home, Vec::new());
    }
    assert!(home.renaming.as_ref().is_some_and(|r| r.version == id(1002)), "F2 renames the card under the pointer");
    frame(&ctx, &mut home, vec![Event::Text("Better name".into())]);
    frame(&ctx, &mut home, vec![key(Key::Enter, true), key(Key::Enter, false)]);
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.take_actions(), [Action::Rename { project: id(2), version: id(1002), title: "Better name".into() }]);
    assert!(home.renaming.is_none());
    // An empty name is refused and the editor stays open.
    home.keys.rename = true;
    for _ in 0..4 {
        frame(&ctx, &mut home, Vec::new());
    }
    assert!(home.renaming.is_some());
    frame(&ctx, &mut home, vec![key(Key::Backspace, true), key(Key::Backspace, false)]);
    frame(&ctx, &mut home, vec![key(Key::Enter, true), key(Key::Enter, false)]);
    frame(&ctx, &mut home, Vec::new());
    assert!(home.take_actions().is_empty());
    assert!(home.renaming.as_ref().is_some_and(|r| r.error.is_some()), "the reason is shown");
    // Escape gives up.
    frame(&ctx, &mut home, vec![key(Key::Escape, true), key(Key::Escape, false)]);
    frame(&ctx, &mut home, Vec::new());
    assert!(home.renaming.is_none());
    assert!(home.take_actions().is_empty());
    // An unchanged name is not a rename.
    home.keys.rename = true;
    for _ in 0..4 {
        frame(&ctx, &mut home, Vec::new());
    }
    frame(&ctx, &mut home, vec![key(Key::Enter, true), key(Key::Enter, false)]);
    frame(&ctx, &mut home, Vec::new());
    assert!(home.take_actions().is_empty() && home.renaming.is_none());
}

#[test]
fn right_click_opens_the_card_menu_and_enter_opens_the_focused_card() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: ready_model(five_cards()), ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    let at = rect_of(&home, id(1001)).center();
    let secondary = |pressed| Event::PointerButton { pos: at, button: PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
    frame(&ctx, &mut home, vec![Event::PointerMoved(at)]);
    frame(&ctx, &mut home, vec![secondary(true)]);
    frame(&ctx, &mut home, vec![secondary(false)]);
    frame(&ctx, &mut home, Vec::new());
    assert!(egui::Popup::is_any_open(&ctx), "the context menu with Open, Rename and Save a copy…");
    assert!(home.take_actions().is_empty(), "opening the menu chooses nothing");
    // Close it by clicking elsewhere, then click the card (it takes the keyboard focus) and press Enter.
    let elsewhere = pos2(5.0, 700.0);
    frame(&ctx, &mut home, vec![Event::PointerMoved(elsewhere), press(elsewhere, true)]);
    frame(&ctx, &mut home, vec![press(elsewhere, false)]);
    let _ = home.take_actions();
    let at = rect_of(&home, id(1002)).center();
    click(&ctx, &mut home, at);
    assert_eq!(home.take_actions(), [Action::Open { project: id(2), version: id(1002) }]);
    frame(&ctx, &mut home, vec![key(Key::Enter, true), key(Key::Enter, false)]);
    assert!(home.take_actions().contains(&Action::Open { project: id(2), version: id(1002) }), "Enter opens the focused card");
}

#[test]
fn ctrl_f_focuses_search_and_escape_leaves_home_only_with_an_editor_behind_it() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: ready_model(five_cards()), ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    home.keys.focus_search = true;
    frame(&ctx, &mut home, Vec::new());
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(ctx.memory(|m| m.focused()), Some(egui::Id::new(SEARCH_ID)));
    home.keys.leave = true;
    frame(&ctx, &mut home, Vec::new());
    assert!(home.take_actions().is_empty(), "nothing behind Home: Esc stays");
    home.model.documents_open = true;
    home.keys.leave = true;
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.take_actions(), [Action::Back]);
}

#[test]
fn the_banners_say_what_is_wrong_and_retry_asks_for_a_sync() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: Model { error: Some("Disk full".into()), ..ready_model(five_cards()) }, ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    let at = button(&home, "Retry").center();
    click(&ctx, &mut home, at);
    assert_eq!(home.take_actions(), [Action::RetrySync]);
    home.model.error = None;
    home.model.connected = false;
    frame(&ctx, &mut home, Vec::new());
    let at = button(&home, "Account").center();
    assert!(home.buttons.iter().filter(|(l, _)| l == "Account").count() == 2, "header and banner");
    click(&ctx, &mut home, at);
    assert!(home.take_actions().contains(&Action::Account));
    // Offline has no button: it is information.
    home.model = Model { net: Net::Offline, ..ready_model(five_cards()) };
    frame(&ctx, &mut home, Vec::new());
    assert!(!home.buttons.iter().any(|(l, _)| l == "Retry"));
}

#[test]
fn the_empty_library_offers_new_and_a_disconnected_one_offers_account() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut home = Home { model: ready_model(Vec::new()), ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    let news = home.buttons.iter().filter(|(l, _)| l == "New").count();
    assert_eq!(news, 2, "the header's and the empty state's");
    let at = home.buttons.iter().filter(|(l, _)| l == "New").map(|(_, r)| r.center()).max_by(|a, b| a.y.total_cmp(&b.y)).unwrap();
    click(&ctx, &mut home, at);
    assert_eq!(home.take_actions(), [Action::New]);
    home.model.connected = false;
    frame(&ctx, &mut home, Vec::new());
    assert_eq!(home.buttons.iter().filter(|(l, _)| l == "Account").count(), 2);
}

#[test]
fn the_grid_fills_the_window_with_whole_columns_about_200_points_wide() {
    assert_eq!(columns(100.0), (1, 200.0), "never narrower than a card");
    assert_eq!(columns(212.0), (1, 212.0));
    assert_eq!(columns(424.0), (2, 206.0));
    assert_eq!(columns(1000.0).0, 4);
    for width in [300.0, 640.0, 900.0, 1200.0, 1800.0, 2560.0] {
        let (cols, w) = columns(width);
        assert!(cols >= 1 && (200.0..=300.0).contains(&w), "{width}: {cols} x {w}");
        assert!(cols as f32 * w + (cols as f32 - 1.0) * GAP <= width + 0.01);
    }
    assert_eq!(columns(f32::NAN), (1, 200.0));
    assert_eq!(columns(f32::INFINITY), (1, 200.0));
}

#[test]
fn previews_are_requested_for_visible_cards_decoded_elsewhere_and_kept_in_bounded_textures() {
    let ctx = context(ThemeKind::ALL[0]);
    let mut cards = five_cards();
    for (i, c) in cards.iter_mut().enumerate() {
        c.preview = (i != 1).then(|| PathBuf::from(format!("/cache/previews/{}", c.version)));
    }
    let mut home = Home { model: ready_model(cards), ..Default::default() };
    frame(&ctx, &mut home, Vec::new());
    frame(&ctx, &mut home, Vec::new());
    let batch = home.previews.take_batch();
    assert_eq!(batch.len(), 4, "the card without a preview file asks for nothing");
    assert!(home.previews.take_batch().is_empty(), "requested once");
    frame(&ctx, &mut home, Vec::new());
    assert!(home.previews.take_batch().is_empty(), "still in flight, not asked again");
    let picture = |w: usize| Decoded { size: [w, 2], rgba: vec![200; w * 2 * 4] };
    home.previews.accept(&ctx, vec![(id(1001), Some(picture(4))), (id(1003), None), (id(1004), Some(picture(2)))]);
    assert_eq!(home.previews.loaded(), 2);
    frame(&ctx, &mut home, Vec::new());
    assert!(home.previews.take_batch().is_empty(), "a preview that failed to decode stays a placeholder");
    // Many previews: the oldest textures go.
    let many: Vec<_> = (0..300u128).map(|n| (id(5000 + n), Some(picture(2)))).collect();
    home.previews.accept(&ctx, many);
    assert!(home.previews.loaded() <= MAX_TEXTURES);
    // A large batch is cut to the decoder's batch size.
    let mut previews = Previews::default();
    for n in 0..40u128 {
        previews.request(id(n), &PathBuf::from("/x"));
    }
    assert_eq!(previews.take_batch().len(), cloud_home::PREVIEW_BATCH);
}
