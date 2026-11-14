//! Application state, toolbar, sidebar, dialogs, and Google sync glue.

use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, Local, NaiveDate, NaiveDateTime, NaiveTime};
use eframe::egui::{self, Align, Align2, Color32, Key, Layout, RichText, Sense, Stroke, Vec2};

use crate::app_icon::AppIcon;
use crate::dates::*;
use crate::google::{self, Cmd, Worker};
use crate::model::{Calendar, Event, Source, Store, CALENDAR_COLORS};
use crate::sync::{Notice, SyncEngine};
use crate::theme::{self, Palette, ACCENT, RED};
use crate::interact::Pointer;
use crate::layout;
use crate::views;

/// How often to pull changes from Google.
const AUTO_SYNC: StdDuration = StdDuration::from_secs(5 * 60);
/// How often to retry while local changes are waiting to upload.
const RETRY_SYNC: StdDuration = StdDuration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Day,
    Week,
    Month,
    Year,
}

impl View {
    const ALL: [View; 4] = [View::Day, View::Week, View::Month, View::Year];
    fn label(self) -> &'static str {
        match self {
            View::Day => "Day",
            View::Week => "Week",
            View::Month => "Month",
            View::Year => "Year",
        }
    }
}

/// Things views ask the app to do once drawing is finished.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Select(Option<String>),
    Open(String),
    New { start: NaiveDateTime, end: NaiveDateTime, all_day: bool },
    GoTo(NaiveDate, View),
    /// A drag is about to change this event (snapshot it for sync).
    BeginEdit(String),
    /// Live update while dragging.
    SetTimes { id: String, start: NaiveDateTime, end: NaiveDateTime },
    /// A drag finished changing this event.
    Committed(String),
}

pub struct Editor {
    pub draft: Event,
    pub is_new: bool,
    start_date: NaiveDate,
    start_time: NaiveTime,
    end_date: NaiveDate,
    end_time: NaiveTime,
    error: Option<String>,
}

impl Editor {
    fn new(draft: Event, is_new: bool) -> Self {
        let end = if draft.all_day { draft.end - Duration::days(1) } else { draft.end };
        Editor {
            start_date: draft.start.date(),
            start_time: draft.start.time(),
            end_date: end.date().max(draft.start.date()),
            end_time: draft.end.time(),
            draft,
            is_new,
            error: None,
        }
    }
}

pub struct App {
    pub store: Store,
    pub view: View,
    pub date: NaiveDate,
    pub today: NaiveDate,
    pub selected: Option<String>,
    pub pointer: Pointer,
    pub scroll_to: Option<f32>,
    pub actions: Vec<Action>,
    editor: Option<Editor>,
    sidebar_open: bool,
    mini_month: NaiveDate,
    worker: Worker,
    engine: SyncEngine,
    last_sync: Option<Instant>,
    status: Option<(String, bool)>,
    show_google: bool,
    icon: AppIcon,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let today = Local::now().date_naive();
        let mut app = App {
            store: Store::load(),
            view: View::Week,
            date: today,
            today,
            selected: None,
            pointer: Pointer::default(),
            scroll_to: Some(layout::HOUR_H * 7.5),
            actions: Vec::new(),
            editor: None,
            sidebar_open: true,
            mini_month: first_of_month(today),
            worker: Worker::spawn(cc.egui_ctx.clone(), google::Endpoints::from_env()),
            engine: SyncEngine::default(),
            last_sync: None,
            status: None,
            show_google: false,
            icon: AppIcon::new(&cc.egui_ctx),
        };
        SyncEngine::prepare(&mut app.store);
        app.sync();
        app
    }

    // ---- Google glue (the logic lives in sync.rs) -------------------------

    fn sync(&mut self) {
        if self.engine.sync(&self.store, self.today) {
            self.last_sync = Some(Instant::now());
        }
        self.flush();
    }

    fn dragging_id(&self) -> Option<String> {
        self.pointer.dragging_id().map(str::to_owned)
    }

    fn handle_messages(&mut self) {
        let mut any = false;
        while let Ok(msg) = self.worker.rx.try_recv() {
            self.worker.busy = self.worker.busy.saturating_sub(1);
            let dragging = self.dragging_id();
            self.engine.handle(&mut self.store, msg, dragging.as_deref(), self.today);
            self.apply_notices();
            any = true;
        }
        if any {
            self.flush();
        }
    }

    /// Reacts to what the sync engine reported.
    fn apply_notices(&mut self) {
        for notice in std::mem::take(&mut self.engine.notices) {
            match notice {
                Notice::Status { text, error } => {
                    if error {
                        eprintln!("Google: {text}");
                    }
                    self.status = Some((text, error));
                }
                Notice::Renamed { from, to } => {
                    if self.selected.as_deref() == Some(from.as_str()) {
                        self.selected = Some(to.clone());
                    }
                    if let Some(ed) = self.editor.as_mut().filter(|ed| ed.draft.id == from) {
                        ed.draft.id = to.clone();
                    }
                    self.pointer.rename(&from, &to);
                }
            }
        }
    }

    /// Hands queued commands to the worker, applies notices, and saves.
    fn flush(&mut self) {
        for cmd in self.engine.outbox.drain(..) {
            self.worker.send(cmd);
        }
        self.apply_notices();
        self.store.save();
    }

    // ---- Event operations -------------------------------------------------

    fn new_event(&mut self, start: NaiveDateTime, end: NaiveDateTime, all_day: bool) {
        let calendar_id = self
            .selected
            .as_ref()
            .and_then(|id| self.store.event(id))
            .map(|e| e.calendar_id.clone())
            .filter(|cid| self.store.calendar(cid).is_some_and(|c| !c.read_only))
            .or_else(|| self.store.writable_calendars().find(|c| c.visible).map(|c| c.id.clone()))
            .or_else(|| self.store.writable_calendars().next().map(|c| c.id.clone()));
        let Some(calendar_id) = calendar_id else { return };
        let draft = Event {
            id: format!("local:{}", uuid::Uuid::new_v4().simple()),
            calendar_id,
            title: String::new(),
            start,
            end,
            all_day,
            location: String::new(),
            notes: String::new(),
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
        };
        self.editor = Some(Editor::new(draft, true));
    }

    fn open_event(&mut self, id: &str) {
        if let Some(ev) = self.store.event(id) {
            self.selected = Some(id.to_owned());
            self.editor = Some(Editor::new(ev.clone(), false));
        }
    }

    fn commit_event(&mut self, mut ev: Event, is_new: bool) {
        if ev.title.trim().is_empty() {
            ev.title = "New Event".into();
        }
        let mut id = ev.id.clone();
        if is_new {
            self.store.events.push(ev);
        } else if let Some(old) = self.store.event(&id).cloned() {
            self.engine.before_change(&mut self.store, &id);
            let old_base = self.store.event(&id).and_then(|e| e.base.clone());
            // Sync bookkeeping lives on the stored copy, not the editor's draft.
            ev.rev = old.rev;
            ev.pending = old.pending;
            ev.remote_id = old.remote_id.clone();
            ev.recurring_id = old.recurring_id.clone();
            ev.etag = old.etag.clone();
            ev.base = old_base;
            // Moving to another calendar means delete + recreate remotely.
            if old.calendar_id != ev.calendar_id {
                self.engine.event_deleted(&mut self.store, &old, false);
                ev.remote_id = None;
                ev.recurring_id = None;
                ev.etag = None;
                ev.base = None;
            }
            if let Some(slot) = self.store.event_mut(&id) {
                *slot = ev;
            }
        } else {
            // Deleted elsewhere while the editor was open: keep the user's
            // version as a new event rather than dropping their edit.
            id = format!("local:{}", uuid::Uuid::new_v4().simple());
            ev.id = id.clone();
            ev.remote_id = None;
            ev.recurring_id = None;
            ev.etag = None;
            ev.base = None;
            self.store.events.push(ev);
            self.status = Some(("That event was deleted elsewhere while you edited it; your version was kept".into(), true));
        }
        self.engine.event_changed(&mut self.store, &id);
        self.selected = Some(id);
        self.flush();
    }

    /// Called when the user starts dragging an event.
    fn begin_edit(&mut self, id: &str) {
        self.engine.before_change(&mut self.store, id);
    }

    fn delete_event(&mut self, id: &str) {
        let Some(ev) = self.store.event(id).cloned() else { return };
        if self.store.calendar(&ev.calendar_id).is_some_and(|c| c.read_only) {
            return;
        }
        self.store.events.retain(|e| e.id != id);
        self.engine.event_deleted(&mut self.store, &ev, true);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
        self.flush();
    }

    fn apply_actions(&mut self) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                Action::Select(id) => self.selected = id,
                Action::Open(id) => self.open_event(&id),
                Action::New { start, end, all_day } => self.new_event(start, end, all_day),
                Action::GoTo(d, v) => self.go_to(d, v),
                Action::BeginEdit(id) => self.begin_edit(&id),
                Action::SetTimes { id, start, end } => {
                    if let Some(ev) = self.store.event_mut(&id) {
                        ev.start = start;
                        ev.end = end;
                    }
                }
                Action::Committed(id) => {
                    self.engine.event_changed(&mut self.store, &id);
                    self.flush();
                }
            }
        }
    }

    // ---- Navigation -------------------------------------------------------

    pub fn go_to(&mut self, date: NaiveDate, view: View) {
        if matches!(view, View::Day | View::Week) && !matches!(self.view, View::Day | View::Week) {
            self.scroll_to = Some(layout::HOUR_H * 7.5);
        }
        self.date = date;
        self.view = view;
        self.mini_month = first_of_month(date);
    }

    fn step(&mut self, dir: i32) {
        self.date = match self.view {
            View::Day => self.date + Duration::days(dir as i64),
            View::Week => self.date + Duration::days(7 * dir as i64),
            View::Month => add_months(self.date, dir),
            View::Year => add_months(self.date, 12 * dir),
        };
        self.mini_month = first_of_month(self.date);
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.wants_keyboard_input();
        let modal = self.editor.is_some() || self.show_google;
        let mut view = None;
        let mut step = 0;
        let mut today = false;
        let mut new = false;
        let mut delete = false;
        ctx.input(|i| {
            let cmd = i.modifiers.command;
            for (n, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4].into_iter().enumerate() {
                if cmd && i.key_pressed(k) {
                    view = Some(View::ALL[n]);
                }
            }
            if cmd && i.key_pressed(Key::T) {
                today = true;
            }
            if cmd && i.key_pressed(Key::N) {
                new = true;
            }
            if !typing {
                if i.key_pressed(Key::ArrowLeft) {
                    step = -1;
                }
                if i.key_pressed(Key::ArrowRight) {
                    step = 1;
                }
                if i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace) {
                    delete = true;
                }
            }
        });
        if modal {
            return;
        }
        if let Some(v) = view {
            self.go_to(self.date, v);
        }
        if today {
            self.go_to(self.today, self.view);
        }
        if step != 0 {
            self.step(step);
        }
        if new {
            let start = midnight(self.date) + Duration::hours(9);
            self.new_event(start, start + Duration::hours(1), false);
        }
        if delete {
            if let Some(id) = self.selected.clone() {
                self.delete_event(&id);
            }
        }
    }

    // ---- UI: toolbar ------------------------------------------------------

    fn toolbar(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::TopBottomPanel::top("toolbar")
            .exact_height(48.0)
            .frame(egui::Frame::none().fill(p.toolbar).inner_margin(egui::Margin::symmetric(12.0, 0.0)))
            .show(ctx, |ui| {
                ui.painter().hline(
                    ui.max_rect().x_range().expand(12.0),
                    ui.max_rect().bottom(),
                    Stroke::new(1.0_f32, p.grid_strong),
                );
                let full = ui.max_rect();
                ui.horizontal_centered(|ui| {
                    if icon_button(ui, "☰", "Show/hide calendar list").clicked() {
                        self.sidebar_open = !self.sidebar_open;
                    }
                    if icon_button(ui, "+", "New event (Ctrl+N)").clicked() {
                        let start = midnight(self.date) + Duration::hours(9);
                        self.new_event(start, start + Duration::hours(1), false);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let connected = self.store.google.is_connected();
                        if connected {
                            let label = if self.worker.busy > 0 { "Syncing…" } else { "⟳" };
                            if icon_button(ui, label, "Sync with Google").clicked() {
                                self.sync();
                            }
                        }
                        let google_label = if connected { "Google: connected" } else { "Connect Google" };
                        if ui.button(google_label).clicked() {
                            self.show_google = true;
                        }
                        let unsynced = self.store.unsynced_count();
                        if connected && unsynced > 0 {
                            let s = if unsynced == 1 { "" } else { "s" };
                            ui.label(RichText::new(format!("{unsynced} change{s} not yet on Google")).color(p.secondary).small());
                        }
                        if let Some((msg, err)) = &self.status {
                            let color = if *err { RED } else { p.secondary };
                            let short: String = msg.chars().take(60).collect();
                            ui.label(RichText::new(short).color(color).small()).on_hover_text(msg);
                        }
                    });
                });
                // Centered segmented control, laid over the row.
                let seg_w = 280.0;
                let seg = egui::Rect::from_center_size(full.center(), Vec2::new(seg_w, 26.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(seg).layout(Layout::left_to_right(Align::Center)));
                if let Some(v) = segmented(&mut child, self.view, p) {
                    self.go_to(self.date, v);
                }
            });
    }

    // ---- UI: sidebar ------------------------------------------------------

    fn sidebar(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(220.0)
            .frame(egui::Frame::none().fill(p.sidebar).inner_margin(egui::Margin::same(14.0)))
            .show_animated(ctx, self.sidebar_open, |ui| {
                ui.painter().vline(
                    ui.max_rect().right() + 14.0,
                    ui.max_rect().y_range().expand(14.0),
                    Stroke::new(1.0_f32, p.grid_strong),
                );
                let mini_h = 210.0;
                let list_h = (ui.available_height() - mini_h).max(80.0);
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, true]).show(ui, |ui| {
                    self.calendar_list(ui, p);
                });
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    ui.add_space(4.0);
                    ui.allocate_ui(Vec2::new(ui.available_width(), mini_h), |ui| {
                        ui.with_layout(Layout::top_down(Align::Min), |ui| self.mini_month_ui(ui, p));
                    });
                });
            });
    }

    fn calendar_list(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut remove: Option<String> = None;
        let groups: [(&str, bool); 2] = [("On My Computer", false), ("Google", true)];
        for (title, google) in groups {
            let idxs: Vec<usize> = (0..self.store.calendars.len())
                .filter(|&i| self.store.calendars[i].google_id().is_some() == google)
                .collect();
            if idxs.is_empty() && google {
                continue;
            }
            let header = if google {
                self.store.google.account_email.clone().unwrap_or_else(|| title.to_owned())
            } else {
                title.to_owned()
            };
            ui.add_space(6.0);
            ui.label(RichText::new(header).font(theme::bold_font(11.5)).color(p.secondary));
            ui.add_space(2.0);
            let local_count = idxs.len();
            for i in idxs {
                let cal = &mut self.store.calendars[i];
                let resp = calendar_row(ui, cal, p);
                if resp.clicked() {
                    cal.visible = !cal.visible;
                    self.store.save();
                    break;
                }
                let cal_id = cal.id.clone();
                resp.context_menu(|ui| {
                    ui.label(RichText::new("Color").small().color(p.secondary));
                    ui.horizontal(|ui| {
                        for c in CALENDAR_COLORS {
                            let (r, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
                            ui.painter().circle_filled(r.center(), 7.0, theme::rgb(c));
                            if resp.clicked() {
                                if let Some(cal) = self.store.calendars.iter_mut().find(|x| x.id == cal_id) {
                                    cal.color = c;
                                }
                                ui.close_menu();
                            }
                        }
                    });
                    if !google && local_count > 1 && ui.button("Delete Calendar").clicked() {
                        remove = Some(cal_id.clone());
                        ui.close_menu();
                    }
                });
            }
            if !google {
                let add = ui.add(egui::Label::new(RichText::new("+  Add Calendar").color(p.secondary)).sense(Sense::click()));
                if add.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    let n = self.store.calendars.len();
                    self.store.calendars.push(Calendar {
                        id: format!("local:{}", uuid::Uuid::new_v4().simple()),
                        name: "Untitled".into(),
                        color: CALENDAR_COLORS[n % CALENDAR_COLORS.len()],
                        visible: true,
                        source: Source::Local,
                        read_only: false,
                    });
                    self.store.save();
                }
            }
        }
        if let Some(id) = remove {
            self.store.calendars.retain(|c| c.id != id);
            self.store.events.retain(|e| e.calendar_id != id);
            self.store.save();
        }
    }

    fn mini_month_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(self.mini_month.format("%B %Y").to_string()).font(theme::bold_font(13.0)).color(p.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if small_button(ui, "›").clicked() {
                    self.mini_month = add_months(self.mini_month, 1);
                }
                if small_button(ui, "‹").clicked() {
                    self.mini_month = add_months(self.mini_month, -1);
                }
            });
        });
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 7.0 * 24.0), Sense::hover());
        if let Some(day) = views::mini_month(ui, rect, self.mini_month, self.today, Some(self.date), p, true) {
            self.date = day;
            if self.view == View::Year {
                self.view = View::Day;
            }
        }
    }

    // ---- UI: header (title + nav) -----------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let (bold, rest) = match self.view {
                View::Day => (self.date.format("%B %-d,").to_string(), self.date.format(" %Y").to_string()),
                View::Week | View::Month => (self.date.format("%B").to_string(), self.date.format(" %Y").to_string()),
                View::Year => (self.date.format("%Y").to_string(), String::new()),
            };
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.label(RichText::new(bold).font(theme::bold_font(26.0)).color(p.text));
            ui.label(RichText::new(rest).font(theme::font(26.0)).color(p.text));
            if self.view == View::Day {
                ui.add_space(12.0);
                ui.label(RichText::new(self.date.format("%A").to_string()).font(theme::font(16.0)).color(p.secondary));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(16.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                if small_button(ui, "›").on_hover_text("Next").clicked() {
                    self.step(1);
                }
                if ui.button("Today").clicked() {
                    self.go_to(self.today, self.view);
                }
                if small_button(ui, "‹").on_hover_text("Previous").clicked() {
                    self.step(-1);
                }
            });
        });
    }

    // ---- UI: dialogs ------------------------------------------------------

    fn editor_window(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(ed) = self.editor.as_mut() else { return };
        let read_only = self.store.calendar(&ed.draft.calendar_id).is_some_and(|c| c.read_only);
        let mut open = true;
        let mut save = false;
        let mut delete = false;
        let mut cancel = false;
        egui::Window::new(if ed.is_new { "New Event" } else { "Event" })
            .id(egui::Id::new("event-editor"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(360.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!read_only, |ui| {
                    let title = ui.add(
                        egui::TextEdit::singleline(&mut ed.draft.title)
                            .hint_text("New Event")
                            .font(theme::bold_font(18.0))
                            .frame(false)
                            .desired_width(f32::INFINITY),
                    );
                    if ed.is_new && !title.has_focus() && ed.draft.title.is_empty() && ui.memory(|m| m.focused().is_none()) {
                        title.request_focus();
                    }
                    if title.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        save = true;
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.draft.location)
                            .hint_text("Add Location")
                            .frame(false)
                            .desired_width(f32::INFINITY),
                    );
                    ui.separator();
                    egui::Grid::new("editor-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label(RichText::new("all-day").color(p.secondary));
                        ui.checkbox(&mut ed.draft.all_day, "");
                        ui.end_row();

                        ui.label(RichText::new("starts").color(p.secondary));
                        ui.horizontal(|ui| {
                            ui.push_id("start-date", |ui| ui.add(egui_extras::DatePickerButton::new(&mut ed.start_date)));
                            if !ed.draft.all_day {
                                time_combo(ui, "start-time", &mut ed.start_time);
                            }
                        });
                        ui.end_row();

                        ui.label(RichText::new("ends").color(p.secondary));
                        ui.horizontal(|ui| {
                            ui.push_id("end-date", |ui| ui.add(egui_extras::DatePickerButton::new(&mut ed.end_date)));
                            if !ed.draft.all_day {
                                time_combo(ui, "end-time", &mut ed.end_time);
                            }
                        });
                        ui.end_row();

                        ui.label(RichText::new("calendar").color(p.secondary));
                        let current = self.store.calendar(&ed.draft.calendar_id);
                        let (name, color) =
                            current.map(|c| (c.name.clone(), theme::rgb(c.color))).unwrap_or(("—".into(), p.secondary));
                        egui::ComboBox::new("calendar-combo", "")
                            .selected_text(RichText::new(format!("● {name}")).color(color))
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for c in self.store.calendars.iter().filter(|c| !c.read_only || read_only) {
                                    let label = RichText::new(format!("● {}", c.name)).color(theme::rgb(c.color));
                                    ui.selectable_value(&mut ed.draft.calendar_id, c.id.clone(), label);
                                }
                            });
                        ui.end_row();
                    });
                    ui.separator();
                    ui.add(
                        egui::TextEdit::multiline(&mut ed.draft.notes)
                            .hint_text("Add Notes")
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                });
                if let Some(err) = &ed.error {
                    ui.label(RichText::new(err).color(RED));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if !ed.is_new && !read_only {
                        if ui.button(RichText::new("Delete").color(RED)).clicked() {
                            delete = true;
                        }
                    }
                    if read_only {
                        ui.label(RichText::new("Read-only calendar").color(p.secondary).small());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !read_only {
                            let btn = egui::Button::new(RichText::new(if ed.is_new { "Add" } else { "Save" }).color(Color32::WHITE))
                                .fill(ACCENT);
                            if ui.add(btn).clicked() {
                                save = true;
                            }
                        }
                        if ui.button(if read_only { "Close" } else { "Cancel" }).clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            cancel = true;
        }
        if save {
            let mut ev = ed.draft.clone();
            if ev.all_day {
                ev.start = midnight(ed.start_date);
                ev.end = midnight(ed.end_date.max(ed.start_date)) + Duration::days(1);
            } else {
                ev.start = ed.start_date.and_time(ed.start_time);
                ev.end = ed.end_date.and_time(ed.end_time);
            }
            if ev.end < ev.start {
                ed.error = Some("The event ends before it starts.".into());
                return;
            }
            let is_new = ed.is_new;
            self.editor = None;
            self.commit_event(ev, is_new);
        } else if delete {
            let id = ed.draft.id.clone();
            self.editor = None;
            self.delete_event(&id);
        } else if cancel || !open {
            self.editor = None;
        }
    }

    fn google_window(&mut self, ctx: &egui::Context, p: &Palette) {
        let mut open = self.show_google;
        let mut connect = false;
        let mut disconnect = false;
        egui::Window::new("Google Calendar")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(440.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                let g = &mut self.store.google;
                if g.is_connected() {
                    ui.label(format!(
                        "Connected{}.",
                        g.account_email.as_ref().map(|e| format!(" as {e}")).unwrap_or_default()
                    ));
                    ui.label(
                        RichText::new("Changes from other devices appear within 5 minutes. Your changes are sent to Google right away; if Google can't be reached they're kept and retried every minute.")
                            .color(p.secondary),
                    );
                    ui.add_space(8.0);
                    if ui.button(RichText::new("Disconnect").color(RED)).clicked() {
                        disconnect = true;
                    }
                    return;
                }
                ui.label("CleanCalendar talks to Google with your own OAuth client:");
                ui.add_space(4.0);
                for step in [
                    "1. In Google Cloud Console, create a project and enable the Google Calendar API.",
                    "2. Configure the OAuth consent screen and add yourself as a test user.",
                    "3. Create Credentials → OAuth client ID → Application type “Desktop app”.",
                    "4. Paste the client ID and secret below, then sign in.",
                ] {
                    ui.label(RichText::new(step).color(p.secondary));
                }
                ui.hyperlink_to("Open Google Cloud Console", "https://console.cloud.google.com/apis/credentials");
                ui.add_space(8.0);
                egui::Grid::new("google-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Client ID");
                    ui.add(egui::TextEdit::singleline(&mut g.client_id).desired_width(300.0));
                    ui.end_row();
                    ui.label("Client secret");
                    ui.add(egui::TextEdit::singleline(&mut g.client_secret).password(true).desired_width(300.0));
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let busy = self.worker.busy > 0;
                    let btn = egui::Button::new(RichText::new("Sign in with Google").color(Color32::WHITE)).fill(ACCENT);
                    if ui.add_enabled(!busy && !g.client_id.trim().is_empty(), btn).clicked() {
                        connect = true;
                    }
                    if busy {
                        ui.spinner();
                        ui.label(RichText::new("Waiting for the browser…").color(p.secondary));
                    }
                });
            });
        self.show_google = open;
        if connect {
            self.store.save();
            let g = &self.store.google;
            self.worker.send(Cmd::Connect { client_id: g.client_id.clone(), client_secret: g.client_secret.clone() });
        }
        if disconnect {
            self.engine.disconnect(&mut self.store);
            self.flush();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        theme::apply_style(ctx);
        let p = Palette::get(ctx);
        self.today = Local::now().date_naive();
        self.icon.update(ctx, frame, self.today);

        self.handle_messages();
        let interval = if self.store.unsynced_count() > 0 { RETRY_SYNC } else { AUTO_SYNC };
        if self.last_sync.is_some_and(|t| t.elapsed() > interval) {
            self.sync();
        }
        self.shortcuts(ctx);

        self.toolbar(ctx, &p);
        self.sidebar(ctx, &p);
        egui::CentralPanel::default().frame(egui::Frame::none().fill(p.bg)).show(ctx, |ui| {
            ui.add_space(10.0);
            self.header(ui, &p);
            ui.add_space(6.0);
            match self.view {
                View::Day => views::time_grid(self, ui, &p, 1),
                View::Week => views::time_grid(self, ui, &p, 7),
                View::Month => views::month(self, ui, &p),
                View::Year => views::year(self, ui, &p),
            }
        });
        self.apply_actions();
        self.editor_window(ctx, &p);
        self.google_window(ctx, &p);

        // Keep the current-time line moving.
        ctx.request_repaint_after(StdDuration::from_secs(30));
    }
}

// ---------------------------------------------------------------------------
// Widgets

fn icon_button(ui: &mut egui::Ui, text: &str, hover: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(15.0)).frame(false).min_size(Vec2::new(28.0, 26.0)))
        .on_hover_text(hover)
}

fn small_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(16.0)).min_size(Vec2::new(26.0, 22.0)))
}

/// macOS-style segmented control.
fn segmented(ui: &mut egui::Ui, current: View, p: &Palette) -> Option<View> {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let track = if p.dark { Color32::from_rgb(0x3A, 0x3A, 0x3C) } else { Color32::from_rgb(0xE3, 0xE3, 0xE8) };
    painter.rect_filled(rect, 7.0, track);
    let w = rect.width() / View::ALL.len() as f32;
    let mut picked = None;
    for (i, v) in View::ALL.into_iter().enumerate() {
        let r = egui::Rect::from_min_size(rect.min + Vec2::new(i as f32 * w, 0.0), Vec2::new(w, rect.height()));
        let resp = ui.interact(r, ui.id().with(("seg", i)), Sense::click());
        if v == current {
            let knob = r.shrink(2.0);
            let fill = if p.dark { Color32::from_rgb(0x63, 0x63, 0x66) } else { Color32::WHITE };
            ui.painter().rect_filled(knob.translate(Vec2::new(0.0, 0.5)), 5.0, Color32::from_black_alpha(30));
            ui.painter().rect_filled(knob, 5.0, fill);
        } else if i > 0 && View::ALL[i - 1] != current {
            ui.painter().vline(r.left(), r.y_range().shrink(6.0), Stroke::new(1.0_f32, p.tertiary));
        }
        ui.painter().text(r.center(), Align2::CENTER_CENTER, v.label(), theme::font(13.0), p.text);
        if resp.clicked() {
            picked = Some(v);
        }
    }
    picked
}

fn calendar_row(ui: &mut egui::Ui, cal: &Calendar, p: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 5.0, p.selection);
    }
    let color = theme::rgb(cal.color);
    let box_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 12.0, rect.center().y), Vec2::splat(14.0));
    if cal.visible {
        ui.painter().rect_filled(box_rect, 3.5, color);
        let a = box_rect.left_center() + Vec2::new(3.0, 0.0);
        let b = box_rect.center_bottom() + Vec2::new(-1.0, -3.5);
        let c = box_rect.right_top() + Vec2::new(-3.0, 3.5);
        let s = Stroke::new(1.8_f32, Color32::WHITE);
        ui.painter().line_segment([a, b], s);
        ui.painter().line_segment([b, c], s);
    } else {
        ui.painter().rect_stroke(box_rect.shrink(0.75), 3.5, Stroke::new(1.5_f32, color));
    }
    let mut name = cal.name.clone();
    if name.chars().count() > 24 {
        name = name.chars().take(23).collect::<String>() + "…";
    }
    ui.painter().text(
        egui::pos2(rect.left() + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        theme::font(13.5),
        p.text,
    );
    resp.on_hover_text(&cal.name)
}

fn time_combo(ui: &mut egui::Ui, id: &str, t: &mut NaiveTime) {
    egui::ComboBox::new(id, "").selected_text(fmt_time(*t)).width(96.0).height(260.0).show_ui(ui, |ui| {
        for q in 0..96 {
            let slot = NaiveTime::from_hms_opt(q / 4, (q % 4) * 15, 0).unwrap();
            ui.selectable_value(t, slot, fmt_time(slot));
        }
    });
}
