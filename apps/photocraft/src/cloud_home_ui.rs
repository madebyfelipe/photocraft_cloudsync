//! The Home screen's egui view: header (title, search, sort, filter chips, New, Account), banners,
//! the Recents row and the responsive project grid. It only draws and records what the person asked
//! for as [`Action`]s; `cloud_sync::DesktopApp` carries them out. Colours, radii and fonts come from
//! the theme tokens, so every theme (dark and light) works.
use crate::cloud_home::{self, Badge, Card, Filter, Sort};
use egui::{Align, Color32, FontId, Key, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use photocraft_ui_egui::{icons, theme};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// The card width the grid aims for; the real width stretches so whole columns fill the window.
const CARD_WIDTH: f32 = 200.0;
const GAP: f32 = 12.0;
const MAX_TEXTURES: usize = 96;
const SKELETON_CARDS: usize = 8;
/// Id of the search field, so Ctrl+F can focus it.
const SEARCH_ID: &str = "cloud-home-search";

/// The connection, as the Home screen needs to know it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Net {
    #[default]
    Idle,
    Syncing,
    Offline,
    Error(String),
}

/// What is happening to one revision on the person's behalf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    Opening,
    Renaming,
    /// The last attempt failed; `rename` is the title to retry with (an open is retried as an open).
    Failed {
        error: String,
        rename: Option<String>,
    },
}

/// Everything the view shows, pushed in by the adapter.
#[derive(Default)]
pub struct Model {
    /// The worker has reported its settings.
    pub ready: bool,
    pub connected: bool,
    pub net: Net,
    /// The first listing from the server (or the decision that there is none) is in.
    pub listing_done: bool,
    /// A message the adapter wants shown with a Retry.
    pub error: Option<String>,
    pub cards: Vec<Card>,
    pub recents: Vec<Uuid>,
    pub cache_bytes: u64,
    pub cache_limit: u64,
    /// By revision.
    pub busy: HashMap<Uuid, Busy>,
    /// "Back to editor" has somewhere to go.
    pub documents_open: bool,
    pub now_ms: u64,
}

/// What the person asked for; the adapter acts on these after the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Open { project: Uuid, version: Uuid },
    Rename { project: Uuid, version: Uuid, title: String },
    SaveCopy { project: Uuid, version: Uuid },
    New,
    Back,
    Account,
    RetrySync,
}

/// Keys the adapter took from the editor's shortcut handling while Home is up.
#[derive(Clone, Copy, Debug, Default)]
pub struct Keys {
    pub focus_search: bool,
    pub rename: bool,
    pub leave: bool,
}

struct Renaming {
    project: Uuid,
    version: Uuid,
    text: String,
    error: Option<&'static str>,
    first_frame: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewState {
    InFlight,
    Failed,
}

/// Preview textures, decoded off the UI thread and kept for the cards on screen (least recently used go first).
#[derive(Default)]
pub struct Previews {
    textures: HashMap<Uuid, (egui::TextureHandle, u64)>,
    state: HashMap<Uuid, PreviewState>,
    want: Vec<(Uuid, PathBuf)>,
    clock: u64,
}

impl Previews {
    pub fn begin_frame(&mut self) {
        self.clock = self.clock.wrapping_add(1);
    }

    fn texture(&mut self, version: &Uuid) -> Option<(egui::TextureId, egui::Vec2)> {
        let clock = self.clock;
        let (handle, stamp) = self.textures.get_mut(version)?;
        *stamp = clock;
        Some((handle.id(), handle.size_vec2()))
    }

    /// A card on screen has a preview file but no texture yet.
    fn request(&mut self, version: Uuid, path: &PathBuf) {
        if !self.textures.contains_key(&version) && !self.state.contains_key(&version) && !self.want.iter().any(|(v, _)| *v == version) {
            self.want.push((version, path.clone()));
        }
    }

    /// The next batch to decode on the worker (marked in flight).
    pub fn take_batch(&mut self) -> Vec<(Uuid, PathBuf)> {
        let n = self.want.len().min(cloud_home::PREVIEW_BATCH);
        let batch: Vec<_> = self.want.drain(..n).collect();
        self.want.clear();
        for (version, _) in &batch {
            self.state.insert(*version, PreviewState::InFlight);
        }
        batch
    }

    /// Decoded previews arrived: textures for the good ones, a placeholder for the rest.
    pub fn accept(&mut self, ctx: &egui::Context, results: Vec<(Uuid, Option<cloud_home::Decoded>)>) {
        for (version, decoded) in results {
            match decoded {
                Some(d) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(d.size, &d.rgba);
                    let handle = ctx.load_texture(format!("cloud-preview-{version}"), image, egui::TextureOptions::LINEAR);
                    self.state.remove(&version);
                    self.textures.insert(version, (handle, self.clock));
                }
                None => {
                    self.state.insert(version, PreviewState::Failed);
                }
            }
        }
        while self.textures.len() > MAX_TEXTURES {
            let oldest = self.textures.iter().min_by_key(|(_, (_, stamp))| *stamp).map(|(v, _)| *v);
            match oldest {
                Some(v) => self.textures.remove(&v),
                None => break,
            };
        }
    }

    #[cfg(test)]
    #[cfg(test)]
    pub fn loaded(&self) -> usize {
        self.textures.len()
    }
}

/// The Home screen: what it shows, what the person typed and what they asked for.
pub struct Home {
    pub model: Model,
    pub search: String,
    pub filter: Filter,
    pub sort: Sort,
    pub keys: Keys,
    pub actions: Vec<Action>,
    pub previews: Previews,
    /// Where the cards and Recents tiles were drawn this frame (what a test clicks).
    pub rects: Vec<(Uuid, Rect)>,
    pub recent_rects: Vec<(Uuid, Rect)>,
    /// Where the header's buttons, chips and banner buttons were drawn this frame, by label.
    pub buttons: Vec<(String, Rect)>,
    renaming: Option<Renaming>,
    focused: Option<Uuid>,
    hovered: Option<Uuid>,
}

impl Default for Home {
    fn default() -> Self {
        Self {
            model: Model::default(),
            search: String::new(),
            filter: Filter::All,
            sort: Sort::LastEdited,
            keys: Keys::default(),
            actions: Vec::new(),
            previews: Previews::default(),
            rects: Vec::new(),
            recent_rects: Vec::new(),
            buttons: Vec::new(),
            renaming: None,
            focused: None,
            hovered: None,
        }
    }
}

/// What a card's menu chose.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Open,
    Rename,
    SaveCopy,
}

fn card_menu(ui: &mut Ui, choice: &mut Option<Choice>) {
    ui.set_min_width(150.0);
    for (label, c) in [("Open", Choice::Open), ("Rename", Choice::Rename), ("Save a copy…", Choice::SaveCopy)] {
        if ui.button(label).clicked() {
            *choice = Some(c);
            ui.close();
        }
    }
}

fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

fn badge_color(badge: Badge, t: &theme::Tokens) -> Color32 {
    match badge {
        Badge::NotUploaded => t.warning,
        Badge::Downloading => t.accent,
        Badge::Conflict { .. } => t.danger,
        Badge::OnThisComputer => t.text_dim,
        Badge::InTheCloud => t.text_faint,
    }
}

/// A filter chip: a pill that is filled when selected.
fn chip(ui: &mut Ui, t: &theme::Tokens, label: &str, selected: bool) -> egui::Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, if selected { t.text } else { t.text_dim });
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(24.0, 10.0), Sense::click());
    let fill = if selected {
        t.accent_soft
    } else if resp.hovered() {
        t.hover
    } else {
        t.field
    };
    let border = if selected { t.accent } else { t.field_border };
    let radius = rect.height() / 2.0;
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().rect_stroke(rect, radius, Stroke::new(1.0, border), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, t.text);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small pill button drawn on a card or banner.
fn pill(ui: &mut Ui, t: &theme::Tokens, label: &str, primary: bool) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(12.5), if primary { t.primary_text } else { t.text });
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(20.0, 8.0), Sense::click());
    let fill = match (primary, resp.hovered()) {
        (true, _) => t.primary_bg,
        (false, true) => t.hover,
        (false, false) => t.field,
    };
    ui.painter().rect_filled(rect, rect.height() / 2.0, fill);
    ui.painter().rect_stroke(rect, rect.height() / 2.0, Stroke::new(1.0, t.field_border), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, t.text);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

impl Home {
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    /// Draws the whole screen into `ui`'s available area.
    pub fn show(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        self.previews.begin_frame();
        self.focused = None;
        self.hovered = None;
        self.rects.clear();
        self.recent_rects.clear();
        self.buttons.clear();
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, t.canvas);
        let inner = area.shrink2(vec2(24.0, 14.0));
        ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
            ui.spacing_mut().item_spacing = vec2(GAP, GAP);
            self.header(ui, &t);
            self.banners(ui, &t);
            self.body(ui, &t);
        });
        self.handle_keys(ui);
    }

    fn handle_keys(&mut self, ui: &Ui) {
        let keys = std::mem::take(&mut self.keys);
        if keys.focus_search {
            ui.ctx().memory_mut(|m| m.request_focus(egui::Id::new(SEARCH_ID)));
        }
        if keys.rename
            && self.renaming.is_none()
            && let Some(version) = self.focused.or(self.hovered)
            && let Some(card) = self.model.cards.iter().find(|c| c.version == version)
        {
            self.renaming = Some(Renaming { project: card.project, version, text: card.title.clone(), error: None, first_frame: true });
        }
        if keys.leave && self.model.documents_open {
            self.actions.push(Action::Back);
        }
    }

    fn header(&mut self, ui: &mut Ui, t: &theme::Tokens) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Projects").font(theme::semibold(24.0)).color(t.text));
            ui.add_space(8.0);
            let search = egui::TextEdit::singleline(&mut self.search)
                .id(egui::Id::new(SEARCH_ID))
                .hint_text("Search projects")
                .desired_width(260.0)
                .margin(vec2(8.0, 6.0));
            ui.add(search);
            egui::ComboBox::from_id_salt("cloud-home-sort").selected_text(self.sort.label()).width(120.0).show_ui(ui, |ui| {
                for sort in Sort::ALL {
                    ui.selectable_value(&mut self.sort, sort, sort.label());
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let new = cloud_home_button(ui, t, "New", true);
                self.buttons.push(("New".into(), new.rect));
                if new.clicked() {
                    self.actions.push(Action::New);
                }
                let account = cloud_home_button(ui, t, "Account", false);
                self.buttons.push(("Account".into(), account.rect));
                if account.clicked() {
                    self.actions.push(Action::Account);
                }
                if self.model.documents_open {
                    let back = cloud_home_button(ui, t, "Back to editor", false);
                    self.buttons.push(("Back to editor".into(), back.rect));
                    if back.clicked() {
                        self.actions.push(Action::Back);
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            for filter in Filter::ALL {
                let chip = chip(ui, t, filter.label(), self.filter == filter);
                self.buttons.push((filter.label().into(), chip.rect));
                if chip.clicked() {
                    self.filter = filter;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(cloud_home::cache_label(self.model.cache_bytes, self.model.cache_limit)).size(12.0).color(t.text_faint));
                let (status, color) = self.status_text(t);
                ui.label(RichText::new(status).size(12.0).color(color));
                if !self.model.ready || (self.model.connected && !self.model.listing_done) {
                    ui.spinner();
                }
            });
        });
    }

    fn status_text(&self, t: &theme::Tokens) -> (&'static str, Color32) {
        if !self.model.ready {
            ("Starting…", t.text_faint)
        } else if self.model.error.is_some() || matches!(self.model.net, Net::Error(_)) {
            ("Sync problem", t.warning)
        } else if !self.model.connected {
            ("Not connected", t.text_faint)
        } else {
            match self.model.net {
                Net::Offline => ("Offline", t.warning),
                Net::Syncing => ("Syncing…", t.text_dim),
                _ => ("Up to date", t.text_dim),
            }
        }
    }

    fn banners(&mut self, ui: &mut Ui, t: &theme::Tokens) {
        let error = self.model.error.clone().or_else(|| if let Net::Error(e) = &self.model.net { Some(e.clone()) } else { None });
        if let Some(message) = error {
            if self.banner(ui, t, t.danger, "triangle-alert", &message, Some("Retry")) {
                self.actions.push(Action::RetrySync);
            }
        } else if self.model.ready && self.model.connected && self.model.net == Net::Offline {
            self.banner(ui, t, t.warning, "cloud", "Offline — showing projects on this computer. Edits are saved locally and upload when you reconnect.", None);
        } else if self.model.ready
            && !self.model.connected
            && !self.model.cards.is_empty()
            && self.banner(ui, t, t.accent, "cloud", "Connect your account to see cloud projects. Projects on this computer are listed below.", Some("Account"))
        {
            self.actions.push(Action::Account);
        }
    }

    /// A one-line message strip; true when its button was pressed.
    fn banner(&mut self, ui: &mut Ui, t: &theme::Tokens, accent: Color32, icon: &str, message: &str, button: Option<&str>) -> bool {
        let mut pressed = false;
        egui::Frame::new().fill(t.card).stroke(Stroke::new(1.0, accent)).corner_radius(t.radius).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                icons::paint(ui, r, icon, 16.0, accent);
                ui.add(egui::Label::new(RichText::new(message).color(t.text)).wrap());
                if let Some(label) = button {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let pill = pill(ui, t, label, false);
                        self.buttons.push((label.into(), pill.rect));
                        pressed = pill.clicked();
                    });
                }
            });
        });
        pressed
    }

    fn body(&mut self, ui: &mut Ui, t: &theme::Tokens) {
        let loading = !self.model.ready || (self.model.connected && !self.model.listing_done && self.model.cards.is_empty());
        if loading {
            self.skeleton(ui, t);
            return;
        }
        if self.model.cards.is_empty() {
            self.empty(ui, t);
            return;
        }
        let shown = cloud_home::visible(&self.model.cards, &self.search, self.filter, self.sort, &self.model.recents);
        if self.filter == Filter::All && self.search.trim().is_empty() {
            let recent = cloud_home::recent_cards(&self.model.cards, &self.model.recents);
            if !recent.is_empty() {
                ui.label(RichText::new("Recents").font(theme::semibold(14.0)).color(t.text_dim));
                self.recents_row(ui, t, &recent);
                ui.label(RichText::new("All projects").font(theme::semibold(14.0)).color(t.text_dim));
            }
        }
        if shown.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No projects match").font(theme::semibold(16.0)).color(t.text_dim));
                ui.label(RichText::new("Try another search or filter.").color(t.text_faint));
            });
            return;
        }
        self.grid(ui, t, &shown);
    }

    fn empty(&mut self, ui: &mut Ui, t: &theme::Tokens) {
        ui.add_space((ui.available_height() * 0.18).max(16.0));
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            icons::paint(ui, r, "cloud", 48.0, t.text_faint);
            ui.add_space(4.0);
            if self.model.connected {
                ui.label(RichText::new("No projects yet").font(theme::semibold(20.0)).color(t.text));
                ui.label(RichText::new("Create a project and it is saved to the cloud as you work.").color(t.text_dim));
            } else {
                ui.label(RichText::new("Connect your account to see cloud projects").font(theme::semibold(20.0)).color(t.text));
                ui.label(RichText::new("Projects you create here are saved on this computer and upload once you connect.").color(t.text_dim));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let width = if self.model.connected { 110.0 } else { 240.0 };
                ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                let new = cloud_home_button(ui, t, "New", true);
                self.buttons.push(("New".into(), new.rect));
                if new.clicked() {
                    self.actions.push(Action::New);
                }
                if !self.model.connected {
                    let account = cloud_home_button(ui, t, "Account", false);
                    self.buttons.push(("Account".into(), account.rect));
                    if account.clicked() {
                        self.actions.push(Action::Account);
                    }
                }
            });
        });
    }

    /// Placeholder cards while the first listing is on its way.
    fn skeleton(&mut self, ui: &mut Ui, t: &theme::Tokens) {
        let (cols, w) = columns(ui.available_width());
        let (thumb_h, card_h) = card_height(w);
        ui.label(RichText::new("Loading your projects…").color(t.text_dim));
        let rows = SKELETON_CARDS.div_ceil(cols);
        for _ in 0..rows {
            ui.horizontal(|ui| {
                for _ in 0..cols {
                    let (rect, _) = ui.allocate_exact_size(vec2(w, card_h), Sense::hover());
                    ui.painter().rect_filled(rect, t.radius, t.card);
                    let thumb = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(w - 16.0, thumb_h));
                    ui.painter().rect_filled(thumb, t.radius_sm, t.field);
                    for (y, frac) in [(thumb.bottom() + 12.0, 0.7), (thumb.bottom() + 34.0, 0.4)] {
                        let bar = Rect::from_min_size(pos2(thumb.left(), y), vec2((w - 16.0) * frac, 10.0));
                        ui.painter().rect_filled(bar, 4.0, t.field);
                    }
                }
            });
        }
    }

    fn recents_row(&mut self, ui: &mut Ui, t: &theme::Tokens, recent: &[usize]) {
        egui::ScrollArea::horizontal().id_salt("cloud-home-recents").auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal(|ui| {
                for &i in recent {
                    if let Some(card) = self.model.cards.get(i).cloned() {
                        self.recent_tile(ui, t, &card);
                    }
                }
            });
            ui.add_space(4.0);
        });
    }

    fn recent_tile(&mut self, ui: &mut Ui, t: &theme::Tokens, card: &Card) {
        let (rect, _) = ui.allocate_exact_size(vec2(236.0, 64.0), Sense::hover());
        self.recent_rects.push((card.version, rect));
        let id = ui.id().with(("recent", card.version));
        let resp = ui.interact(rect, id, Sense::click());
        if resp.has_focus() {
            self.focused = Some(card.version);
        }
        if resp.hovered() {
            self.hovered = Some(card.version);
        }
        let border = if resp.has_focus() || resp.hovered() { t.accent } else { t.card_border };
        ui.painter().rect_filled(rect, t.radius, t.card);
        ui.painter().rect_stroke(rect, t.radius, Stroke::new(1.0, border), StrokeKind::Inside);
        let thumb = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(64.0, 48.0));
        self.paint_thumb(ui, t, thumb, card, true);
        let x = thumb.right() + 10.0;
        let max_w = rect.right() - x - 8.0;
        let title = elided(ui, &card.title, theme::medium(13.0), t.text, max_w);
        ui.painter().galley(pos2(x, rect.top() + 12.0), title, t.text);
        let when = elided(ui, &cloud_home::edited_label(card.created_ms, self.model.now_ms), FontId::proportional(11.5), t.text_faint, max_w);
        ui.painter().galley(pos2(x, rect.top() + 34.0), when, t.text_faint);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &card.title));
        let mut choice = None;
        if resp.clicked() {
            choice = Some(Choice::Open);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).context_menu(|ui| card_menu(ui, &mut choice));
        self.apply_choice(choice, card);
    }

    fn grid(&mut self, ui: &mut Ui, t: &theme::Tokens, shown: &[usize]) {
        let (cols, w) = columns(ui.available_width() - 6.0);
        let (thumb_h, card_h) = card_height(w);
        let rows = shown.len().div_ceil(cols);
        egui::ScrollArea::vertical().id_salt("cloud-home-grid").auto_shrink([false, false]).show_rows(ui, card_h, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let Some(card) = shown.get(row * cols + col).and_then(|&i| self.model.cards.get(i)).cloned() else { break };
                        self.card(ui, t, &card, vec2(w, card_h), thumb_h);
                    }
                });
            }
        });
    }

    fn card(&mut self, ui: &mut Ui, t: &theme::Tokens, card: &Card, size: egui::Vec2, thumb_h: f32) {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        self.rects.push((card.version, rect));
        let renaming = self.renaming.as_ref().is_some_and(|r| r.version == card.version);
        let id = ui.id().with(("card", card.version));
        let resp = ui.interact(rect, id, if renaming { Sense::hover() } else { Sense::click() });
        let focused = resp.has_focus();
        if focused {
            self.focused = Some(card.version);
        }
        if resp.hovered() {
            self.hovered = Some(card.version);
        }
        let hot = resp.hovered() || focused;
        ui.painter().rect_filled(rect, t.radius, t.card);
        let border = if focused {
            Stroke::new(2.0, t.accent)
        } else if hot {
            Stroke::new(1.0, t.accent)
        } else {
            Stroke::new(1.0, t.card_border)
        };
        ui.painter().rect_stroke(rect, t.radius, border, StrokeKind::Inside);
        let thumb = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(rect.width() - 16.0, thumb_h));
        self.paint_thumb(ui, t, thumb, card, false);
        let busy = self.model.busy.get(&card.version).cloned();
        let mut choice = None;
        match &busy {
            Some(Busy::Opening | Busy::Renaming) => {
                ui.painter().rect_filled(thumb, t.radius_sm, t.scrim);
                egui::Spinner::new().size(26.0).color(t.text).paint_at(ui, Rect::from_center_size(thumb.center(), vec2(26.0, 26.0)));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            Some(Busy::Failed { error, rename }) => {
                ui.painter().rect_filled(thumb, t.radius_sm, t.scrim);
                let text = elided_wrapped(ui, error, t.text, thumb.width() - 20.0);
                let text_h = text.size().y;
                ui.painter().galley(pos2(thumb.left() + 10.0, thumb.top() + 8.0), text, t.text);
                let retry = Rect::from_center_size(pos2(thumb.center().x, (thumb.top() + 16.0 + text_h + 14.0).min(thumb.bottom() - 14.0)), vec2(70.0, 24.0));
                let r = ui.interact(retry, id.with("retry"), Sense::click());
                ui.painter().rect_filled(retry, 12.0, if r.hovered() { t.hover } else { t.field });
                ui.painter().text(retry.center(), egui::Align2::CENTER_CENTER, "Retry", FontId::proportional(12.5), t.text);
                if r.clicked() {
                    match rename {
                        Some(title) => self.actions.push(Action::Rename { project: card.project, version: card.version, title: title.clone() }),
                        None => self.actions.push(Action::Open { project: card.project, version: card.version }),
                    }
                }
            }
            None => {}
        }
        // The "more" button on hover or focus.
        if hot && !renaming && busy.is_none() {
            let more_rect = Rect::from_center_size(pos2(thumb.right() - 16.0, thumb.top() + 16.0), vec2(26.0, 26.0));
            let more = ui.interact(more_rect, id.with("more"), Sense::click());
            ui.painter().rect_filled(more_rect, 13.0, if more.hovered() { t.hover } else { t.chrome });
            icons::paint(ui, more_rect, "ellipsis", 16.0, t.icon);
            more.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "More actions"));
            egui::Popup::menu(&more).show(|ui| card_menu(ui, &mut choice));
        }
        // Title, edit time and badge.
        let x = rect.left() + 12.0;
        let max_w = rect.width() - 24.0;
        let title_y = thumb.bottom() + 8.0;
        if renaming {
            self.rename_field(ui, t, card, Rect::from_min_size(pos2(x - 4.0, title_y - 2.0), vec2(max_w + 8.0, 24.0)));
        } else {
            let g = elided(ui, &card.title, theme::medium(14.0), t.text, max_w);
            ui.painter().galley(pos2(x, title_y), g, t.text);
        }
        let when_y = title_y + 24.0;
        let open_w = if card.open { 40.0 } else { 0.0 };
        let when = elided(ui, &cloud_home::edited_label(card.created_ms, self.model.now_ms), FontId::proportional(12.0), t.text_faint, max_w - open_w);
        ui.painter().galley(pos2(x, when_y), when, t.text_faint);
        if card.open {
            ui.painter().text(pos2(rect.right() - 12.0, when_y + 7.0), egui::Align2::RIGHT_CENTER, "Open", FontId::proportional(11.5), t.accent_text);
        }
        let badge_y = when_y + 22.0;
        let color = badge_color(card.badge, t);
        let label_color = if matches!(card.badge, Badge::OnThisComputer | Badge::InTheCloud) { t.text_dim } else { color };
        ui.painter().circle_filled(pos2(x + 4.0, badge_y + 7.0), 4.0, color);
        let label = elided(ui, &card.badge.label(), FontId::proportional(12.0), label_color, max_w - 16.0);
        ui.painter().galley(pos2(x + 14.0, badge_y), label, label_color);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &card.title));
        if resp.clicked() && busy.is_none() {
            resp.request_focus();
            choice = Some(Choice::Open);
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        if !renaming {
            resp.context_menu(|ui| card_menu(ui, &mut choice));
        }
        if focused && !renaming && busy.is_none() && ui.input(|i| i.key_pressed(Key::Enter)) {
            choice = Some(Choice::Open);
        }
        // A card that is busy still lets you rename it elsewhere, but not open it twice.
        if busy.is_none() || choice == Some(Choice::Rename) {
            self.apply_choice(choice, card);
        }
    }

    fn paint_thumb(&mut self, ui: &mut Ui, t: &theme::Tokens, thumb: Rect, card: &Card, small: bool) {
        ui.painter().rect_filled(thumb, t.radius_sm, t.field);
        let visible = ui.is_rect_visible(thumb);
        if let Some((id, size)) = self.previews.texture(&card.version) {
            let scale = (thumb.width() / size.x.max(1.0)).min(thumb.height() / size.y.max(1.0));
            let image = Rect::from_center_size(thumb.center(), size * scale);
            ui.painter().image(id, image, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            icons::paint(ui, thumb, "image", if small { 20.0 } else { 36.0 }, t.text_faint);
            if visible && let Some(path) = &card.preview {
                self.previews.request(card.version, path);
            }
        }
    }

    fn rename_field(&mut self, ui: &mut Ui, t: &theme::Tokens, card: &Card, rect: Rect) {
        let Some(r) = self.renaming.as_mut() else { return };
        let edit_id = ui.id().with(("rename", card.version));
        let mut child = ui.new_child(UiBuilder::new().max_rect(rect));
        let output =
            egui::TextEdit::singleline(&mut r.text).id(edit_id).desired_width(rect.width()).margin(vec2(4.0, 3.0)).font(theme::medium(14.0)).show(&mut child);
        let resp = output.response;
        if r.first_frame {
            resp.request_focus();
            let mut state = output.state;
            state.cursor.set_char_range(Some(egui::text::CCursorRange::select_all(&output.galley)));
            state.store(ui.ctx(), edit_id);
            r.first_frame = false;
        }
        let escape = (resp.has_focus() || resp.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape));
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let clicked_away = resp.lost_focus() && !enter && !escape;
        if let Some(why) = r.error {
            ui.painter().text(pos2(rect.left(), rect.bottom() + 2.0), egui::Align2::LEFT_TOP, why, FontId::proportional(11.0), t.danger);
        }
        if escape {
            self.renaming = None;
        } else if enter || clicked_away {
            match cloud_home::validate_title(&r.text) {
                Ok(title) => {
                    let (project, version) = (r.project, r.version);
                    self.renaming = None;
                    if title != card.title {
                        self.actions.push(Action::Rename { project, version, title });
                    }
                }
                Err(why) if enter => {
                    r.error = Some(why);
                    resp.request_focus();
                }
                Err(_) => self.renaming = None,
            }
        }
    }

    fn apply_choice(&mut self, choice: Option<Choice>, card: &Card) {
        match choice {
            Some(Choice::Open) => self.actions.push(Action::Open { project: card.project, version: card.version }),
            Some(Choice::Rename) => {
                self.renaming = Some(Renaming { project: card.project, version: card.version, text: card.title.clone(), error: None, first_frame: true });
            }
            Some(Choice::SaveCopy) => self.actions.push(Action::SaveCopy { project: card.project, version: card.version }),
            None => {}
        }
    }
}

/// A toolbar button in the header: the primary one is filled.
fn cloud_home_button(ui: &mut Ui, t: &theme::Tokens, label: &str, primary: bool) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), theme::medium(13.0), if primary { t.primary_text } else { t.text });
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 28.0, 30.0), Sense::click());
    let fill = match (primary, resp.hovered()) {
        (true, true) => t.primary_bg.gamma_multiply(0.9),
        (true, false) => t.primary_bg,
        (false, true) => t.hover,
        (false, false) => t.field,
    };
    ui.painter().rect_filled(rect, t.radius, fill);
    if !primary {
        ui.painter().rect_stroke(rect, t.radius, Stroke::new(1.0, t.field_border), StrokeKind::Inside);
    }
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, t.text);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn elided_wrapped(ui: &Ui, text: &str, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), FontId::proportional(12.0), color, max_w.max(1.0));
    job.wrap.max_rows = 3;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}

/// Columns and card width for a row `available` points wide.
pub fn columns(available: f32) -> (usize, f32) {
    let available = if available.is_finite() { available.max(CARD_WIDTH) } else { CARD_WIDTH };
    let cols = (((available + GAP) / (CARD_WIDTH + GAP)).floor() as usize).max(1);
    let width = ((available - GAP * (cols as f32 - 1.0)) / cols as f32).floor().max(CARD_WIDTH.min(available));
    (cols, width)
}

/// Thumbnail height and total card height for a card `width` wide.
fn card_height(width: f32) -> (f32, f32) {
    let thumb = (width * 0.68).round();
    (thumb, thumb + 16.0 + 8.0 + 24.0 + 22.0 + 24.0)
}

#[cfg(test)]
mod tests;
