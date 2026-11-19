//! Application state, toolbar, sidebar, dialogs, and sync glue.

use std::collections::HashMap;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime};
use eframe::egui::{self, Align, Align2, Color32, Key, Layout, RichText, Sense, Stroke, Vec2};

use crate::app_icon::AppIcon;
use crate::editor::{self, Editor, Outcome, Placement, Scope};
use crate::dates::*;
use crate::google;
use crate::menu::{self, Command, ContextMenu, Icon, Item, MenuAt};
use crate::model::{Calendar, Event, Source, Store, CALENDAR_COLORS};
use crate::remote::{Worker, GOOGLE};
use crate::recurrence;
use crate::settings::{self, WeekScroll};
use crate::sync::{Notice, SyncEngine};
use crate::theme::{self, Palette, RED};
use crate::interact::Pointer;
use crate::views;

mod search_ui;
mod settings_window;

/// How often to retry while local changes are waiting to upload. (How
/// often to pull changes is the "Refresh Calendars" setting.)
const RETRY_SYNC: StdDuration = StdDuration::from_secs(60);

/// The calendar's views. The last one shown is remembered (see
/// [`crate::settings::Settings::view`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    Day,
    #[default]
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
    /// Open the right-click menu for what's at `pos`.
    ContextMenu { at: MenuAt, pos: egui::Pos2 },
}

pub struct App {
    pub store: Store,
    pub view: View,
    pub date: NaiveDate,
    pub today: NaiveDate,
    pub selected: Option<String>,
    /// The calendar of the event last selected or edited, kept after
    /// clicking away: new events go on it, as on macOS.
    current_calendar: Option<String>,
    pub pointer: Pointer,
    /// Scroll the day/week grid to this hour on the next frame.
    pub scroll_to: Option<f32>,
    /// The week view's first day while scrolling by days (see
    /// [`App::grid_first`]).
    week_first: Option<NaiveDate>,
    /// While stepping by months or years: the day of the month to go back
    /// to, so 31 Jan → Feb → Mar lands on 31 Mar, not 28 Mar.
    month_anchor: Option<u32>,
    /// Where the month view has been scrolled to; `None` shows `date`'s
    /// month at rest. Navigating (‹ ›, Today, a date) clears it.
    pub month_scroll: Option<views::MonthState>,
    pub actions: Vec<Action>,
    /// The event being edited (in a popover, or the day view's inspector).
    pub editor: Option<Editor>,
    /// Where each event was drawn this frame, to anchor the popover.
    pub event_rects: HashMap<String, egui::Rect>,
    /// The last pointer position, to anchor the popover of a new event.
    last_pointer: Option<egui::Pos2>,
    /// The open right-click menu, and its items (worked out when it opened).
    pub menu: Option<ContextMenu>,
    menu_items: Vec<Item>,
    /// The event last copied or cut, for pasting.
    clipboard: Option<Event>,
    sidebar_open: bool,
    mini_month: NaiveDate,
    worker: Worker,
    engine: SyncEngine,
    last_sync: Option<Instant>,
    status: Option<(String, bool)>,
    settings_ui: settings_window::SettingsUi,
    icon: AppIcon,
    search: SearchUi,
}

/// The toolbar's search field and its results (see [`crate::search`]).
#[derive(Default)]
struct SearchUi {
    /// The field is shown (Ctrl+F, or the magnifier button).
    open: bool,
    query: String,
    /// Give the field the keyboard focus on the next frame.
    focus: bool,
    /// The query the results list was last scrolled for: a new one scrolls
    /// it to what's coming up.
    scrolled_for: Option<String>,
    /// The last results (the list is drawn every frame).
    cache: crate::search::Cache,
}

impl SearchUi {
    /// Whether the results list is shown.
    fn showing(&self) -> bool {
        self.open && !self.query.trim().is_empty()
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let today = Local::now().date_naive();
        let store = Store::load();
        store.settings.apply();
        let mut app = App {
            scroll_to: Some(store.settings.scroll_hour()),
            view: store.settings.view,
            store,
            date: today,
            today,
            selected: None,
            current_calendar: None,
            pointer: Pointer::default(),
            week_first: None,
            month_anchor: None,
            month_scroll: None,
            actions: Vec::new(),
            editor: None,
            event_rects: HashMap::new(),
            last_pointer: None,
            menu: None,
            menu_items: Vec::new(),
            clipboard: None,
            sidebar_open: true,
            mini_month: first_of_month(today),
            worker: Worker::spawn(cc.egui_ctx.clone(), google::Endpoints::from_env()),
            engine: SyncEngine::default(),
            last_sync: None,
            status: None,
            settings_ui: Default::default(),
            icon: AppIcon::new(&cc.egui_ctx),
            search: SearchUi::default(),
        };
        if let Some((view, date)) = std::env::var("CLEANCALENDAR_START").ok().and_then(|s| parse_start(&s)) {
            app.go_to(date.unwrap_or(today), view);
        }
        if let Some(q) = std::env::var("CLEANCALENDAR_SEARCH").ok().filter(|q| !q.trim().is_empty()) {
            app.search = SearchUi { open: true, query: q, ..SearchUi::default() };
        }
        SyncEngine::prepare(&mut app.store);
        app.sync();
        app
    }

    // ---- Sync glue (the logic lives in sync.rs) ---------------------------

    fn sync(&mut self) {
        // A Google account switched off in Settings isn't among the accounts
        // synced (see `Store::accounts`); its changes wait until it's back on.
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
                        eprintln!("Sync: {text}");
                    }
                    self.status = Some((text, error));
                }
                Notice::ClearStatus => self.status = None,
                Notice::Renamed { from, to } => self.rename_shown(&from, &to),
            }
        }
    }

    /// An event's id changed: follow it in the selection, the editor and the
    /// pointer, including ids of a renamed series' occurrences.
    fn rename_shown(&mut self, from: &str, to: &str) {
        let renamed = |id: &str| -> Option<String> {
            if id == from {
                return Some(to.to_owned());
            }
            match recurrence::split_occurrence(id) {
                Some((series, stamp)) if series == from => Some(recurrence::occurrence_id(to, stamp)),
                _ => None,
            }
        };
        if let Some(new) = self.selected.as_deref().and_then(renamed) {
            self.selected = Some(new);
        }
        if let Some(ed) = self.editor.as_mut() {
            if let Some(new) = renamed(&ed.draft.id) {
                ed.draft.id = new;
            }
        }
        if let Some(old) = self.dragging_id() {
            if let Some(new) = renamed(&old) {
                self.pointer.rename(&old, &new);
            }
        }
        self.pointer.rename(from, to);
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

    /// The calendar of the event being edited, else of the selected one.
    fn calendar_in_focus(&self) -> Option<String> {
        self.editor
            .as_ref()
            .map(|ed| ed.draft.calendar_id.clone())
            .or_else(|| self.selected.as_ref().and_then(|id| self.store.find_event(id)).map(|e| e.calendar_id))
    }

    /// Remembers the calendar in focus, so that clicking away (as a
    /// double-click on empty space does first) doesn't forget it.
    fn note_current_calendar(&mut self) {
        if let Some(cal) = self.calendar_in_focus() {
            self.current_calendar = Some(cal);
        }
    }

    /// The calendar new events go on: as on macOS, that of the event
    /// selected or edited (or last so), else the "Default Calendar"
    /// setting, else the first visible one that can be written to.
    fn default_calendar(&self) -> Option<String> {
        pick_calendar(&self.store, self.calendar_in_focus(), self.current_calendar.clone())
    }

    fn is_writable(&self, calendar_id: &str) -> bool {
        self.store.calendar(calendar_id).is_some_and(|c| !c.read_only)
    }

    fn new_event(&mut self, start: NaiveDateTime, end: NaiveDateTime, all_day: bool) {
        let Some(calendar_id) = self.default_calendar() else { return };
        let draft = Event {
            id: format!("local:{}", uuid::Uuid::new_v4().simple()),
            calendar_id,
            title: String::new(),
            start,
            end,
            all_day,
            location: String::new(),
            notes: String::new(),
            attendees: Vec::new(),
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
            recurrence: None,
            time_zone: None,
        };
        self.finish_editor(Outcome::Done);
        if self.editor.is_some() {
            // The open editor can't be saved yet; it shows why.
            return;
        }
        let placement = if self.view == View::Day {
            Placement::Inspector
        } else {
            let anchor = self.last_pointer.map(|p| egui::Rect::from_center_size(p, Vec2::splat(2.0)));
            Placement::Popover { anchor }
        };
        self.selected = Some(draft.id.clone());
        self.editor = Some(Editor::new(draft, true, placement));
    }

    fn open_event(&mut self, id: &str) {
        if self.editor.as_ref().is_some_and(|ed| ed.draft.id == id) {
            return;
        }
        self.finish_editor(Outcome::Done);
        if self.editor.is_some() {
            return;
        }
        if let Some(ev) = self.store.find_event(id) {
            let placement = if self.view == View::Day {
                Placement::Inspector
            } else {
                Placement::Popover { anchor: self.event_rects.get(id).copied() }
            };
            self.selected = Some(id.to_owned());
            self.editor = Some(Editor::new(ev, false, placement));
        }
    }

    /// Closes the editor: keeps the changes (if any), discards them, or
    /// deletes the event.
    fn finish_editor(&mut self, outcome: Outcome) {
        let Some(ed) = self.editor.take() else { return };
        match outcome {
            Outcome::Open => self.editor = Some(ed),
            Outcome::Cancel => {}
            Outcome::Delete => self.delete_event(&ed.draft.id),
            Outcome::DeleteSeries => self.delete_series(&ed.draft.id),
            Outcome::Done => {
                let stored = self.store.find_event(&ed.draft.id);
                if !(ed.is_new || editor::changed(&ed, stored.as_ref())) {
                    return;
                }
                match ed.result() {
                    Ok(ev) if is_occurrence(&self.store, &ev.id) => self.commit_occurrence(ev, ed.scope),
                    Ok(ev) => self.commit_event(recurrence::aligned(ev), ed.is_new),
                    // Keep it open so the problem can be fixed.
                    Err(_) => self.editor = Some(ed),
                }
            }
        }
    }

    /// Saves an edited occurrence of a repeating event: on its own (it
    /// leaves the series), or as a change to the whole series. Changing the
    /// repeat rule always changes the whole series.
    fn commit_occurrence(&mut self, ev: Event, scope: Scope) {
        let Some((series_id, stamp)) = recurrence::split_occurrence(&ev.id) else { return };
        let series_id = series_id.to_owned();
        let Some(series) = self.store.event(&series_id).cloned() else { return };
        let Some(rule) = series.recurrence.clone() else { return };
        let rule_changed = ev.recurrence.as_ref().map_or(true, |r| !r.rule_eq(&rule));
        if scope == Scope::ThisEvent && !rule_changed {
            let id = self.detach(&series_id, stamp, ev);
            self.selected = Some(id);
            return;
        }

        let (s, shown) = recurrence::edit_series(&series, stamp, &ev);
        // If the series' first day isn't one of its weekdays any more, it
        // moves to its first occurrence, and so does the selection on it.
        let unaligned_first = recurrence::occurrence_id(&s.id, recurrence::series_start(&s));
        let s = recurrence::aligned(s);
        let shown = if shown == unaligned_first {
            recurrence::occurrence_id(&s.id, recurrence::series_start(&s))
        } else {
            shown
        };
        self.commit_event(s, false);
        self.selected = Some(shown);
    }

    /// Takes one occurrence out of its series as an event of its own (with
    /// `ev`'s details) and returns the new event's id.
    fn detach(&mut self, series_id: &str, stamp: NaiveDateTime, mut ev: Event) -> String {
        if let Some(mut series) = self.store.event(series_id).cloned() {
            if let Some(rule) = series.recurrence.as_mut() {
                rule.exceptions.push(stamp);
            }
            self.commit_event(series, false);
        }
        let id = format!("local:{}", uuid::Uuid::new_v4().simple());
        ev.id = id.clone();
        ev.recurrence = None;
        // Still listed as part of the series (for "Delete All Events").
        ev.recurring_id = Some(series_id.to_owned());
        ev.remote_id = None;
        ev.etag = None;
        ev.base = None;
        ev.pending = false;
        ev.rev = 0;
        self.store.events.push(ev);
        self.engine.event_changed(&mut self.store, &id);
        self.flush();
        id
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
            // An event with guests stays where it is (the editor's calendar
            // picker can still ask; see `Event::can_change_calendar`).
            if old.calendar_id != ev.calendar_id && !old.can_change_calendar() {
                ev.calendar_id = old.calendar_id.clone();
                self.status = Some((
                    format!("“{}” has invitees, so it stays on its calendar (moving it would re-invite everyone)", ev.title),
                    true,
                ));
            }
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
        self.selected = Some(self.store.event(&id).map_or(id, Event::shown_id));
        self.flush();
    }

    /// Called when the user starts dragging an event. Returns the id to
    /// drag: dragging one occurrence of a repeating event takes it out of
    /// the series first (like Google, a drag changes only that one).
    fn begin_edit(&mut self, id: &str) -> String {
        if is_occurrence(&self.store, id) {
            let Some(ev) = self.store.find_event(id) else { return id.to_owned() };
            let Some((series_id, stamp)) = recurrence::split_occurrence(id) else { return id.to_owned() };
            let new_id = self.detach(&series_id.to_owned(), stamp, ev);
            if let Some(ed) = self.editor.take() {
                // Re-open the editor on the detached event, unsaved edits kept.
                if ed.draft.id == id {
                    let mut draft = ed.draft.clone();
                    draft.id = new_id.clone();
                    draft.recurrence = None;
                    draft.recurring_id = Some(series_id.to_owned());
                    self.editor = Some(Editor::new(draft, false, ed.placement));
                } else {
                    self.editor = Some(ed);
                }
            }
            self.selected = Some(new_id.clone());
            self.pointer.rename(id, &new_id);
            return new_id;
        }
        self.engine.before_change(&mut self.store, id);
        id.to_owned()
    }

    fn is_read_only(&self, ev: &Event) -> bool {
        self.store.calendar(&ev.calendar_id).is_some_and(|c| c.read_only)
    }

    /// Deletes an event, or just this occurrence of a repeating one.
    fn delete_event(&mut self, id: &str) {
        let Some(ev) = self.store.find_event(id) else { return };
        if self.is_read_only(&ev) {
            return;
        }
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
        // An editor left open on it (e.g. the day view's inspector when
        // Delete is pressed) would save it right back when it closes.
        self.close_editor_if(|draft| draft.id == id);
        if is_occurrence(&self.store, id) {
            let Some((series_id, stamp)) = recurrence::split_occurrence(id) else { return };
            let Some(mut series) = self.store.event(series_id).cloned() else { return };
            if let Some(rule) = series.recurrence.as_mut() {
                rule.exceptions.push(stamp);
            }
            self.commit_event(series, false);
            self.selected = None;
            return;
        }
        self.store.events.retain(|e| e.id != id);
        self.engine.event_deleted(&mut self.store, &ev, true);
        self.flush();
    }

    /// Drops the editor, unsaved, when its draft matches (its event is gone).
    fn close_editor_if(&mut self, gone: impl Fn(&Event) -> bool) {
        if self.editor.as_ref().is_some_and(|ed| gone(&ed.draft)) {
            self.editor = None;
        }
    }

    /// Deletes every occurrence of the repeating event `id` belongs to.
    fn delete_series(&mut self, id: &str) {
        let Some(ev) = self.store.find_event(id) else { return };
        if self.is_read_only(&ev) {
            return;
        }
        // A series stored here (shown as occurrences, plus any that were
        // changed on their own), or one a service sent as separate
        // occurrences (their `recurring_id` is the series' remote id).
        let local_series = match recurrence::split_occurrence(id) {
            Some((series, _)) => Some(series.to_owned()),
            None => ev.recurring_id.clone().filter(|r| self.store.event(r).is_some_and(|s| s.recurrence.is_some())),
        };
        let series_id = match (&local_series, &ev.recurring_id) {
            (Some(s), _) | (None, Some(s)) => s.clone(),
            (None, None) => return self.delete_event(id),
        };
        // Its remote id, once uploaded: occurrences the service sent carry it.
        let series_remote = match &local_series {
            Some(s) => self.store.event(s).and_then(|e| e.remote_id.clone()),
            None => Some(series_id.clone()),
        };
        let in_series = |e: &Event| {
            e.id == series_id
                || (e.calendar_id == ev.calendar_id
                    && (e.recurring_id.as_deref() == Some(&series_id)
                        || (series_remote.is_some()
                            && (e.remote_id == series_remote || e.recurring_id == series_remote))))
        };
        let members: Vec<Event> = self.store.events.iter().filter(|e| in_series(e)).cloned().collect();
        self.store.events.retain(|e| !members.iter().any(|m| m.id == e.id));
        self.close_editor_if(|draft| {
            draft.id == id
                || members.iter().any(|m| m.id == draft.id)
                || recurrence::split_occurrence(&draft.id).is_some_and(|(s, _)| s == series_id)
        });
        match (&local_series, self.store.remote_of(&ev)) {
            (Some(_), _) => {
                for m in &members {
                    // Occurrences sent by the service go with their series.
                    if m.recurring_id.is_some() && m.recurring_id == series_remote {
                        continue;
                    }
                    self.engine.event_deleted(&mut self.store, m, true);
                }
            }
            (None, Some(cal)) => self.engine.series_deleted(&mut self.store, cal, &series_id),
            (None, None) => {}
        }
        self.selected = None;
        self.flush();
    }

    // ---- The right-click menu and the edit commands -----------------------

    /// What the right-click menu offers for `at`, as in macOS Calendar.
    fn menu_items_for(&self, at: &MenuAt) -> Vec<Item> {
        let can_create = self.store.writable_calendars().next().is_some();
        let can_paste = can_create && self.clipboard.is_some();
        let id = match at {
            MenuAt::Event(id) => id,
            MenuAt::Time(_) | MenuAt::Day { .. } => {
                return vec![
                    Item::new(Icon::Plus, "New Event", can_create, Command::NewEvent),
                    Item::Separator,
                    Item::new(Icon::Paste, "Paste", can_paste, Command::Paste),
                ];
            }
        };
        let Some(ev) = self.store.find_event(id) else { return Vec::new() };
        let writable = !self.is_read_only(&ev);
        let mut items = vec![
            Item::new(Icon::Info, "Get Info", true, Command::GetInfo),
            Item::Calendars { enabled: writable && ev.can_change_calendar(), current: ev.calendar_id.clone() },
            Item::Separator,
            Item::new(Icon::Trash, "Delete", writable, Command::Delete),
            Item::new(Icon::Scissors, "Cut", writable, Command::Cut),
            Item::new(Icon::Copy, "Copy", true, Command::Copy),
            Item::new(Icon::Paste, "Paste", can_paste, Command::Paste),
            Item::new(Icon::Duplicate, "Duplicate", can_create, Command::Duplicate),
        ];
        if ev.repeats() {
            let next = self.store.adjacent_occurrence(id, true).is_some();
            let prev = self.store.adjacent_occurrence(id, false).is_some();
            items.extend([
                Item::Separator,
                Item::new(Icon::Next, "Go to Next Occurrence", next, Command::NextOccurrence).shortcut(cmd_key!("]")),
                Item::new(Icon::Previous, "Go to Previous Occurrence", prev, Command::PreviousOccurrence).shortcut(cmd_key!("[")),
            ]);
        }
        items.push(Item::Separator);
        items.push(Item::new(Icon::Envelope, "Email Event", true, Command::EmailEvent));
        if ev.repeats() {
            items.push(Item::new(Icon::Envelopes, "Email Event Series", true, Command::EmailSeries));
        }
        // Events don't have participants here (yet), so these stay greyed
        // out, as macOS shows them for events without any.
        let none = Command::EmailEvent;
        items.push(Item::new(Icon::EnvelopePerson, "Email All Participants", false, none.clone()));
        items.push(Item::new(Icon::Bubble, "Message All Participants", false, none));
        items
    }

    /// Draws the open right-click menu and runs what's picked from it.
    fn context_menu(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(m) = self.menu.as_mut() else { return };
        match menu::show(ctx, m, &self.menu_items, &self.store, p) {
            menu::Outcome::Open => {}
            menu::Outcome::Close => self.menu = None,
            menu::Outcome::Run(cmd) => {
                let at = self.menu.take().map(|m| m.at);
                if let Some(at) = at {
                    self.run_command(ctx, at, cmd);
                }
                ctx.request_repaint();
            }
        }
    }

    /// Runs a command from the right-click menu or the keyboard.
    fn run_command(&mut self, ctx: &egui::Context, at: MenuAt, cmd: Command) {
        match (at, cmd) {
            (MenuAt::Event(id), Command::GetInfo) => self.open_event(&id),
            (MenuAt::Event(id), Command::MoveTo(cal)) => self.move_to_calendar(&id, &cal),
            (MenuAt::Event(id), Command::Delete) => {
                self.finish_editor(Outcome::Done);
                self.delete_event(&id);
            }
            (MenuAt::Event(id), Command::Cut) => {
                self.finish_editor(Outcome::Done);
                if self.store.find_event(&id).is_some_and(|ev| !self.is_read_only(&ev)) {
                    self.copy_event(ctx, &id);
                    self.delete_event(&id);
                }
            }
            (MenuAt::Event(id), Command::Copy) => self.copy_event(ctx, &id),
            // Onto the right-clicked event's day, at the copied event's time.
            (MenuAt::Event(id), Command::Paste) => {
                if let Some(ev) = self.store.find_event(&id) {
                    self.paste(ev.start.date(), None);
                }
            }
            (MenuAt::Event(id), Command::Duplicate) => self.duplicate(&id),
            (MenuAt::Event(id), Command::NextOccurrence) => self.go_to_occurrence(&id, true),
            (MenuAt::Event(id), Command::PreviousOccurrence) => self.go_to_occurrence(&id, false),
            (MenuAt::Event(id), Command::EmailEvent) => self.email(&id, false),
            (MenuAt::Event(id), Command::EmailSeries) => self.email(&id, true),
            (MenuAt::Time(t), Command::NewEvent) => self.new_event(t, t + Duration::hours(1), false),
            (MenuAt::Day { day, all_day: true }, Command::NewEvent) => {
                self.new_event(midnight(day), midnight(day) + Duration::days(1), true)
            }
            (MenuAt::Day { day, all_day: false }, Command::NewEvent) => {
                let start = midnight(day) + Duration::hours(9);
                self.new_event(start, start + Duration::hours(1), false);
            }
            (MenuAt::Time(t), Command::Paste) => self.paste(t.date(), Some(t)),
            (MenuAt::Day { day, .. }, Command::Paste) => self.paste(day, None),
            _ => {}
        }
        // The day view's inspector follows the selection.
        if self.view == View::Day && self.editor.is_none() {
            if let Some(id) = self.selected.clone() {
                self.open_event(&id);
            }
        }
    }

    /// Moves an event to another calendar. A repeating event moves as a
    /// whole series (as in macOS Calendar).
    fn move_to_calendar(&mut self, id: &str, calendar_id: &str) {
        self.finish_editor(Outcome::Done);
        let target = match recurrence::split_occurrence(id) {
            Some((series, _)) if is_occurrence(&self.store, id) => series.to_owned(),
            _ => id.to_owned(),
        };
        let Some(mut ev) = self.store.event(&target).cloned() else { return };
        if ev.calendar_id == calendar_id
            || self.is_read_only(&ev)
            || !self.is_writable(calendar_id)
            || !ev.can_change_calendar()
        {
            return;
        }
        ev.calendar_id = calendar_id.to_owned();
        self.commit_event(ev, false);
        self.selected = Some(id.to_owned());
    }

    /// Copies an event for pasting, and as text to the system clipboard.
    fn copy_event(&mut self, ctx: &egui::Context, id: &str) {
        if let Some(ev) = self.store.find_event(id) {
            ctx.copy_text(menu::event_text(&ev, false));
            self.clipboard = Some(ev);
        }
    }

    /// A calendar a copy of an event can go on: its own if possible.
    fn calendar_for_copy(&self, preferred: &str) -> Option<String> {
        Some(preferred.to_owned()).filter(|c| self.is_writable(c)).or_else(|| self.default_calendar())
    }

    /// Pastes the copied event on `day`: at `time` if given (a slot in the
    /// hour grid), else at the time it had. An all-day event stays all-day.
    fn paste(&mut self, day: NaiveDate, time: Option<NaiveDateTime>) {
        let Some(src) = self.clipboard.clone() else { return };
        let start = match (src.all_day, time) {
            (true, _) => midnight(day),
            (false, Some(t)) => t,
            (false, None) => day.and_time(src.start.time()),
        };
        let Some(calendar_id) = self.calendar_for_copy(&src.calendar_id) else { return };
        self.finish_editor(Outcome::Done);
        self.commit_event(src.copy_as_new(calendar_id, start), true);
    }

    /// A copy of the event at the same time, selected (macOS's Duplicate).
    fn duplicate(&mut self, id: &str) {
        let Some(ev) = self.store.find_event(id) else { return };
        let Some(calendar_id) = self.calendar_for_copy(&ev.calendar_id) else { return };
        self.finish_editor(Outcome::Done);
        self.commit_event(ev.copy_as_new(calendar_id, ev.start), true);
    }

    /// Selects and shows the next (or previous) occurrence of a repeating event.
    fn go_to_occurrence(&mut self, id: &str, forward: bool) {
        let Some(occ) = self.store.adjacent_occurrence(id, forward) else { return };
        self.finish_editor(Outcome::Done);
        self.go_to(occ.start.date(), self.view);
        if matches!(self.view, View::Day | View::Week) && !occ.is_banner() {
            let hours = minute_of_day(occ.start) as f32 / 60.0;
            self.scroll_to = Some((hours - 1.0).max(0.0));
        }
        self.selected = Some(occ.id);
    }

    /// Opens a new message in the mail app with the event's details.
    fn email(&mut self, id: &str, series: bool) {
        let Some(ev) = self.store.find_event(id) else { return };
        let url = menu::mailto(&ev.title, &menu::event_text(&ev, series));
        if let Err(e) = open::that(&url) {
            self.status = Some((format!("Could not open your mail app: {e}"), true));
        }
    }

    fn apply_actions(&mut self) {
        // Occurrences detached by a drag get a new id; later actions in this
        // batch may still name the old one.
        let mut renamed: HashMap<String, String> = HashMap::new();
        for mut action in std::mem::take(&mut self.actions) {
            if let Action::SetTimes { id, .. } | Action::Committed(id) = &mut action {
                if let Some(new) = renamed.get(id) {
                    *id = new.clone();
                }
            }
            match action {
                Action::Select(id) => {
                    // The day view's inspector follows the selection.
                    if self.view == View::Day {
                        match &id {
                            Some(id) => self.open_event(id),
                            None => self.finish_editor(Outcome::Done),
                        }
                    }
                    self.selected = id;
                }
                Action::Open(id) => self.open_event(&id),
                Action::New { start, end, all_day } => self.new_event(start, end, all_day),
                Action::GoTo(d, v) => self.go_to(d, v),
                Action::BeginEdit(id) => {
                    let new = self.begin_edit(&id);
                    if new != id {
                        renamed.insert(id, new);
                    }
                }
                Action::SetTimes { id, start, end } => {
                    if let Some(ev) = self.store.event_mut(&id) {
                        ev.start = start;
                        ev.end = end;
                    }
                    // Dragging the event being edited (e.g. a new one) moves the draft too.
                    if let Some(ed) = self.editor.as_mut().filter(|ed| ed.draft.id == id) {
                        ed.set_times(start, end);
                    }
                }
                Action::Committed(id) => {
                    self.engine.event_changed(&mut self.store, &id);
                    self.flush();
                }
                Action::ContextMenu { at, pos } => {
                    self.menu_items = self.menu_items_for(&at);
                    self.menu = (!self.menu_items.is_empty()).then(|| ContextMenu::new(at, pos));
                }
            }
        }
    }

    // ---- Navigation -------------------------------------------------------

    pub fn go_to(&mut self, date: NaiveDate, view: View) {
        if matches!(view, View::Day | View::Week) && !matches!(self.view, View::Day | View::Week) {
            self.scroll_to = Some(self.store.settings.scroll_hour());
        }
        self.date = date;
        self.month_anchor = None;
        self.month_scroll = None;
        if view != self.view {
            self.finish_editor(Outcome::Done);
            // Still open (it can't be saved as is): the inspector only
            // exists in the day view, so show it as a popover elsewhere.
            if let Some(ed) = self.editor.as_mut().filter(|_| view != View::Day) {
                if ed.placement == Placement::Inspector {
                    ed.placement = Placement::Popover { anchor: None };
                }
            }
        }
        self.view = view;
        self.mini_month = first_of_month(date);
    }

    /// Scrolling the month view brought `month` into the middle: show it
    /// (title, sidebar) as if stepped to, without moving the view.
    pub fn month_scrolled_to(&mut self, month: NaiveDate) {
        let anchor = *self.month_anchor.get_or_insert(self.date.day());
        self.date = step_months(month, 0, anchor);
        self.mini_month = first_of_month(month);
    }

    /// Days the week view shows (the "Days per week" setting).
    fn week_days(&self) -> i64 {
        self.store.settings.week_days()
    }

    /// The first day shown in the day/week grid. A week normally starts on
    /// the first day of the week (Monday, for a 5-day week). When scrolling
    /// by days, it starts wherever scrolling left it, as long as the current
    /// date is still in view.
    pub fn grid_first(&mut self, ndays: i64) -> NaiveDate {
        if ndays == 1 {
            return self.date;
        }
        let in_view = |first: NaiveDate| self.date >= first && self.date < first + Duration::days(ndays);
        if self.store.settings.week_scroll == WeekScroll::Day {
            if let Some(first) = self.week_first.filter(|&f| in_view(f)) {
                return first;
            }
        }
        let mut first = start_of_week(self.date);
        if ndays == 5 {
            first += Duration::days(((7 - first.weekday().num_days_from_monday()) % 7) as i64);
        }
        self.week_first = Some(first);
        first
    }

    fn step(&mut self, dir: i32) {
        let by_day = self.store.settings.week_scroll == WeekScroll::Day;
        self.date = match self.view {
            View::Day => self.date + Duration::days(dir as i64),
            View::Week if by_day => {
                let n = self.week_days();
                let first = self.grid_first(n) + Duration::days(dir as i64);
                self.week_first = Some(first);
                (self.date + Duration::days(dir as i64)).clamp(first, first + Duration::days(n - 1))
            }
            View::Week => self.date + Duration::days(7 * dir as i64),
            View::Month | View::Year => {
                let anchor = *self.month_anchor.get_or_insert(self.date.day());
                let months = if self.view == View::Year { 12 * dir } else { dir };
                step_months(self.date, months, anchor)
            }
        };
        self.month_scroll = None;
        if !matches!(self.view, View::Month | View::Year) {
            self.month_anchor = None;
        }
        self.mini_month = first_of_month(self.date);
        // Don't leave an event selected (and deletable with Backspace)
        // that's no longer on screen.
        if self.editor.is_none() {
            self.selected = None;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.wants_keyboard_input();
        let popover = self.editor.as_ref().is_some_and(|ed| matches!(ed.placement, Placement::Popover { .. }));
        let modal = popover || self.settings_ui.open || self.menu.is_some();
        let mut view = None;
        let mut step = 0;
        let mut today = false;
        let mut new = false;
        let mut delete = false;
        let mut edit: Option<Command> = None;
        let mut settings = false;
        let mut find = false;
        ctx.input(|i| {
            if !typing {
                // Ctrl+X/C/V arrive as clipboard events, not key presses.
                for e in &i.events {
                    match e {
                        egui::Event::Cut => edit = Some(Command::Cut),
                        egui::Event::Copy => edit = Some(Command::Copy),
                        egui::Event::Paste(_) => edit = Some(Command::Paste),
                        _ => {}
                    }
                }
                let cmd = i.modifiers.command;
                if cmd && i.key_pressed(Key::D) {
                    edit = Some(Command::Duplicate);
                }
                if cmd && i.key_pressed(Key::CloseBracket) {
                    edit = Some(Command::NextOccurrence);
                }
                if cmd && i.key_pressed(Key::OpenBracket) {
                    edit = Some(Command::PreviousOccurrence);
                }
            }
            let cmd = i.modifiers.command;
            if cmd && i.key_pressed(Key::Comma) {
                settings = true;
            }
            for (n, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4].into_iter().enumerate() {
                if cmd && i.key_pressed(k) {
                    view = Some(View::ALL[n]);
                }
            }
            if cmd && i.key_pressed(Key::T) {
                today = true;
            }
            if cmd && i.key_pressed(Key::F) {
                find = true;
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
        if settings && !popover {
            self.open_settings();
        }
        if modal {
            return;
        }
        if find {
            self.search.open = true;
            self.search.focus = true;
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
            self.new_event(start, start + default_event_len(), false);
        }
        if delete {
            if let Some(id) = self.selected.clone() {
                self.delete_event(&id);
            }
        }
        match (edit, self.selected.clone()) {
            // Onto the selected event's day (else the day shown), at the
            // copied event's time.
            (Some(Command::Paste), selected) => {
                let day = selected.and_then(|id| self.store.find_event(&id)).map_or(self.date, |ev| ev.start.date());
                self.paste(day, None);
            }
            (Some(cmd), Some(id)) => self.run_command(ctx, MenuAt::Event(id), cmd),
            _ => {}
        }
    }

    // ---- UI: toolbar ------------------------------------------------------

    fn toolbar(&mut self, ctx: &egui::Context, p: &Palette) {
        // Like macOS, the toolbar is part of the window: same background, no
        // divider line.
        egui::TopBottomPanel::top("toolbar")
            .exact_height(52.0)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.toolbar).inner_margin(egui::Margin::symmetric(12.0, 0.0)))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.horizontal_centered(|ui| {
                    if icon_button(ui, "☰", "Show/hide calendar list").clicked() {
                        self.sidebar_open = !self.sidebar_open;
                    }
                    if pill_button(ui, "+", p.seg_track, p).on_hover_text(format!("New event ({})", cmd_key!("N"))).clicked() {
                        let start = midnight(self.date) + Duration::hours(9);
                        self.new_event(start, start + default_event_len(), false);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.search_field(ui, p);
                        if icon_button(ui, "⚙", &format!("Settings ({})", cmd_key!(","))).clicked() {
                            self.open_settings();
                        }
                        let connected = !self.store.accounts().is_empty();
                        if connected {
                            if self.worker.busy > 0 {
                                ui.add_sized(Vec2::new(28.0, 26.0), egui::Spinner::new().size(14.0).color(p.secondary))
                                    .on_hover_text("Syncing…");
                            } else if icon_button(ui, "⟳", "Sync now").clicked() {
                                self.sync();
                            }
                        }
                        let unsynced = self.store.unsynced_count();
                        if connected && unsynced > 0 {
                            let s = if unsynced == 1 { "" } else { "s" };
                            ui.label(RichText::new(format!("{unsynced} change{s} not yet synced")).color(p.secondary).small());
                        }
                        if let Some((msg, err)) = &self.status {
                            let color = if *err { RED } else { p.secondary };
                            let short: String = msg.chars().take(60).collect();
                            ui.label(RichText::new(short).color(color).small()).on_hover_text(msg);
                        }
                    });
                });
                // Centered on the whole window (like macOS), laid over the row.
                let seg_w = 300.0;
                let window_mid = ui.ctx().screen_rect().center().x;
                let seg = egui::Rect::from_center_size(egui::pos2(window_mid, full.center().y), Vec2::new(seg_w, 32.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(seg).layout(Layout::left_to_right(Align::Center)));
                if let Some(v) = segmented(&mut child, self.view, p) {
                    self.go_to(self.date, v);
                }
            });
    }

    // ---- UI: sidebar ------------------------------------------------------

    fn sidebar(&mut self, ctx: &egui::Context, p: &Palette) {
        // An inset, rounded, darker panel running the window's full height
        // (it's laid out before the toolbar), like the macOS sidebar.
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(236.0)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.bg).inner_margin(egui::Margin::same(8.0)))
            .show_animated(ctx, self.sidebar_open, |ui| {
                let card = ui.max_rect();
                ui.painter().rect_filled(card, 12.0, p.sidebar);
                if !p.dark {
                    ui.painter().rect_stroke(card, 12.0, Stroke::new(1.0_f32, p.grid));
                }
                let inner = card.shrink2(Vec2::new(14.0, 14.0));
                let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
                let ui = &mut ui;
                // Leave room for the window's title-bar controls area.
                ui.add_space(30.0);
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
        // Local calendars, then one group per account (an account without
        // calendars isn't shown), in the order they've been dragged into.
        let mut groups: Vec<Option<String>> = vec![None, Some(GOOGLE.to_owned())];
        groups.extend(self.store.caldav.iter().map(|a| Some(a.id.clone())));
        fn listed(store: &Store, account: Option<&str>) -> Vec<usize> {
            (0..store.calendars.len())
                .filter(|&i| store.calendars[i].account() == account)
                .filter(|&i| store.is_listed(&store.calendars[i]))
                .collect()
        }
        groups.retain(|a| a.is_none() || !listed(&self.store, a.as_deref()).is_empty());
        let keys: Vec<String> = groups.iter().map(|a| settings::group_key(a.as_deref()).to_owned()).collect();
        let keys = self.store.settings.ordered_groups(keys);
        // Each group's extent (heading to last row), to place a dragged one.
        let mut extents: Vec<egui::Rangef> = Vec::new();
        let mut dragging: Option<usize> = None;
        let mut dropped = false;
        for (gi, key) in keys.iter().enumerate() {
            let account = (key != settings::LOCAL_GROUP).then_some(key.as_str());
            let google = account.is_some();
            let idxs = listed(&self.store, account);
            let description = self.store.settings.google_description.trim();
            let header = match account {
                None => "On My Computer".to_owned(),
                Some(GOOGLE) if !description.is_empty() => description.to_owned(),
                Some(GOOGLE) => self.store.google.account_email.clone().unwrap_or_else(|| "Google".to_owned()),
                Some(a) => self.store.account_name(a),
            };
            let top = ui.cursor().top();
            ui.add_space(6.0);
            let collapsed = self.store.settings.is_collapsed(key);
            let head = group_header(ui, key, &header, collapsed, p);
            if head.clicked() {
                self.store.settings.toggle_collapsed(key);
                self.store.save();
            }
            if head.dragged() || head.drag_stopped() {
                dragging = Some(gi);
                dropped = head.drag_stopped();
            }
            ui.add_space(2.0);
            if collapsed {
                extents.push(egui::Rangef::new(top, ui.cursor().top()));
                continue;
            }
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
            extents.push(egui::Rangef::new(top, ui.cursor().top()));
        }
        // A heading being dragged: a red line (with a ring at its start, as
        // on macOS) marks the gap it would go to, and dropping moves it there.
        if let (Some(from), Some(pos)) = (dragging, ui.ctx().pointer_latest_pos()) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let to = extents.iter().filter(|r| r.center() < pos.y).count();
            if to != from && to != from + 1 {
                if dropped {
                    self.store.settings.move_group(&keys, from, to);
                    self.store.save();
                } else {
                    let y = match to {
                        0 => extents[0].min + 2.0,
                        n => extents[n - 1].max + 1.0,
                    };
                    let x = ui.min_rect().left() + 4.0;
                    let right = ui.max_rect().right() - 4.0;
                    let stroke = Stroke::new(2.0_f32, RED);
                    let painter = ui.painter();
                    painter.circle_stroke(egui::pos2(x, y), 3.5, stroke);
                    painter.line_segment([egui::pos2(x + 3.5, y), egui::pos2(right, y)], stroke);
                }
            }
        }
        if let Some(id) = remove {
            self.close_editor_if(|draft| draft.calendar_id == id);
            if self.selected.as_ref().and_then(|s| self.store.find_event(s)).is_some_and(|e| e.calendar_id == id) {
                self.selected = None;
            }
            self.store.calendars.retain(|c| c.id != id);
            self.store.events.retain(|e| e.calendar_id != id);
            self.store.save();
        }
    }

    fn mini_month_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // "‹  November 2026  ›" with the month name centered, as in macOS.
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
        ui.painter().text(
            row.center(),
            Align2::CENTER_CENTER,
            self.mini_month.format("%B %Y").to_string(),
            theme::font(14.0),
            p.text,
        );
        let mut left = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(Layout::left_to_right(Align::Center)));
        if chevron_button(&mut left, "‹", p).clicked() {
            self.mini_month = add_months(self.mini_month, -1);
        }
        let mut right = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)));
        if chevron_button(&mut right, "›", p).clicked() {
            self.mini_month = add_months(self.mini_month, 1);
        }
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 7.0 * 24.0), Sense::hover());
        if let Some(day) = views::mini_month(ui, rect, self.mini_month, self.today, Some(self.date), p, true, None) {
            self.date = day;
            self.month_anchor = None;
            if self.view == View::Year {
                self.view = View::Day;
            }
        }
    }

    // ---- UI: header (title + nav) -----------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // "November 2026" (bold month), "13 November 2026" in the day view,
        // and a plain "2026" in the year view, like macOS Calendar.
        let (bold, rest) = match self.view {
            View::Day => (self.date.format("%-d %B").to_string(), self.date.format(" %Y").to_string()),
            View::Week | View::Month => (self.date.format("%B").to_string(), self.date.format(" %Y").to_string()),
            View::Year => (String::new(), self.date.format("%Y").to_string()),
        };
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            ui.spacing_mut().item_spacing.x = 0.0;
            if !bold.is_empty() {
                ui.label(RichText::new(bold).font(theme::bold_font(30.0)).color(p.text));
            }
            ui.label(RichText::new(rest).font(theme::font(30.0)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                if pill_button(ui, "›", p.button, p).on_hover_text("Next").clicked() {
                    self.step(1);
                }
                if pill_button(ui, "Today", p.button, p).clicked() {
                    self.go_to(self.today, self.view);
                }
                if pill_button(ui, "‹", p.button, p).on_hover_text("Previous").clicked() {
                    self.step(-1);
                }
            });
        });
        if self.view == View::Day {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new(self.date.format("%A").to_string()).font(theme::font(20.0)).color(p.secondary));
            });
        }
    }

    // ---- UI: dialogs ------------------------------------------------------

    /// Shows the editor's popover (the inspector draws itself in the day view).
    fn editor_popover(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(ed) = self.editor.as_mut() else { return };
        let Placement::Popover { anchor } = ed.placement else { return };
        // Follow the event if it's drawn (e.g. a new event appears after the
        // first frame, or the view scrolls).
        let anchor = self.event_rects.get(&ed.draft.id).copied().or(anchor);
        ed.placement = Placement::Popover { anchor };
        let outcome = editor::popover(ctx, ed, &self.store, p, anchor);
        self.finish_editor(outcome);
    }

    /// The day view's right-hand panel: a mini-month, then the selected
    /// event's details (like macOS Calendar's inspector).
    fn inspector(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::SidePanel::right("inspector")
            .resizable(false)
            .exact_width(320.0)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.bg).inner_margin(egui::Margin { left: 12.0, right: 14.0, top: 8.0, bottom: 12.0 }))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().vline(r.left() - 12.0, r.y_range().expand(20.0), Stroke::new(1.0_f32, p.grid));
                let w = ui.available_width();
                let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 7.0 * 24.0), Sense::hover());
                if let Some(day) = views::mini_month(ui, rect, first_of_month(self.date), self.today, Some(self.date), p, true, None) {
                    self.go_to(day, View::Day);
                }
                ui.add_space(14.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.editor.as_mut() {
                    Some(ed) if ed.placement == Placement::Inspector => {
                        let outcome = editor::inspector(ui, ed, &self.store, p);
                        self.finish_editor(outcome);
                    }
                    _ => {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No Event Selected").font(theme::font(15.0)).color(p.placeholder));
                        });
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        theme::apply_style(ctx);
        let p = Palette::get(ctx);
        self.today = Local::now().date_naive();
        self.icon.update(ctx, frame, self.today);
        self.store.settings.apply();

        self.handle_messages();
        self.note_current_calendar();
        // Unsent changes are retried every minute even when refreshing is
        // manual; otherwise pull as often as "Refresh Calendars" says.
        let refresh = self.store.settings.refresh_minutes;
        let interval = if self.store.unsynced_count() > 0 {
            Some(RETRY_SYNC)
        } else {
            (refresh > 0).then(|| StdDuration::from_secs(refresh as u64 * 60))
        };
        if interval.is_some_and(|i| self.last_sync.is_some_and(|t| t.elapsed() > i)) {
            self.sync();
        }
        self.shortcuts(ctx);

        // Sidebar first, so it runs the window's full height.
        self.sidebar(ctx, &p);
        self.toolbar(ctx, &p);
        if self.search.showing() {
            self.search_results(ctx, &p);
        }
        if self.view == View::Day {
            self.inspector(ctx, &p);
        }
        self.last_pointer = ctx.input(|i| i.pointer.interact_pos()).or(self.last_pointer);
        self.event_rects.clear();
        egui::CentralPanel::default().frame(egui::Frame::none().fill(p.bg)).show(ctx, |ui| {
            ui.add_space(10.0);
            self.header(ui, &p);
            ui.add_space(6.0);
            match self.view {
                View::Day => views::time_grid(self, ui, &p, 1),
                View::Week => {
                    let n = self.week_days();
                    views::time_grid(self, ui, &p, n)
                }
                View::Month => views::month(self, ui, &p),
                View::Year => views::year(self, ui, &p),
            }
        });
        // Actions (e.g. finishing a drag-to-create) change what the views
        // show, but they're applied after the views were drawn this frame.
        // Draw again right away rather than waiting for the next input.
        if !self.actions.is_empty() {
            ctx.request_repaint();
        }
        self.apply_actions();
        self.editor_popover(ctx, &p);
        // "Download location data…" in the editor's location field.
        if self.editor.as_mut().is_some_and(|ed| std::mem::take(&mut ed.open_location_settings)) {
            self.open_settings_at(settings_window::Tab::Locations);
        }
        self.context_menu(ctx, &p);
        self.settings_window(ctx, &p);

        // Remember the view, so the app opens in it next time.
        if self.store.settings.view != self.view {
            self.store.settings.view = self.view;
            self.store.save();
        }

        // Keep the current-time line moving.
        ctx.request_repaint_after(StdDuration::from_secs(30));
    }
}

// ---------------------------------------------------------------------------
// Widgets

/// `CLEANCALENDAR_START=month:2026-11-14` (or just `day`, `week`, `month`,
/// `year`): where to open, for screenshots and testing by hand.
fn parse_start(s: &str) -> Option<(View, Option<NaiveDate>)> {
    let (view, date) = match s.split_once(':') {
        Some((v, d)) => (v, Some(NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok()?)),
        None => (s, None),
    };
    let view = View::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(view.trim()))?;
    Some((view, date))
}

/// Whether `id` names an occurrence of a repeating event stored here (rather
/// than an event of its own).
fn is_occurrence(store: &Store, id: &str) -> bool {
    store.event(id).is_none() && recurrence::split_occurrence(id).is_some()
}

/// The calendar new events go on, as on macOS: that of the event selected
/// or being edited (`in_focus`), else the one last selected (`remembered`),
/// else the "Default Calendar" setting, else the first one shown. The
/// remembered and default calendars only count while they're shown: an
/// event made on a calendar unticked since would be invisible, while it's
/// being edited and after.
fn pick_calendar(store: &Store, in_focus: Option<String>, remembered: Option<String>) -> Option<String> {
    let writable = |cid: &String| store.writable_calendars().any(|c| &c.id == cid);
    let shown = |cid: &String| writable(cid) && store.is_visible(cid);
    in_focus
        .filter(writable)
        .or_else(|| remembered.filter(shown))
        .or_else(|| store.settings.default_calendar.clone().filter(shown))
        .or_else(|| store.writable_calendars().find(|c| c.visible).map(|c| c.id.clone()))
        .or_else(|| store.writable_calendars().next().map(|c| c.id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_events_never_go_on_a_calendar_hidden_since() {
        let mut store = Store::with_defaults();
        let (home, work) = (store.calendars[0].id.clone(), store.calendars[1].id.clone());
        let pick = |s: &Store, remembered: Option<&str>| pick_calendar(s, None, remembered.map(str::to_owned));
        assert_eq!(pick(&store, Some(&work)), Some(work.clone()), "the calendar last selected");

        // "Work" is unticked after its event was selected and deselected.
        store.calendars[1].visible = false;
        assert_eq!(pick(&store, Some(&work)), Some(home.clone()), "not the hidden one");
        store.settings.default_calendar = Some(work.clone());
        assert_eq!(pick(&store, None), Some(home.clone()), "nor as the Default Calendar");

        // An event selected right now keeps its calendar (it's on screen).
        assert_eq!(pick_calendar(&store, Some(home.clone()), Some(work)), Some(home));
    }
}

fn icon_button(ui: &mut egui::Ui, text: &str, hover: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(15.0)).frame(false).min_size(Vec2::new(28.0, 26.0)))
        .on_hover_text(hover)
}

/// A fully rounded button like macOS toolbar buttons: a circle for a single
/// glyph ("+", "‹"), a pill for words ("Today").
fn pill_button(ui: &mut egui::Ui, text: &str, fill: Color32, p: &Palette) -> egui::Response {
    let font = theme::font(if text.chars().count() == 1 { 18.0 } else { 14.0 });
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), p.text);
    let h = 30.0;
    let w = (galley.size().x + 26.0).max(h);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        theme::blend(fill, p.text, 0.18)
    } else if resp.hovered() {
        theme::blend(fill, p.text, 0.08)
    } else {
        fill
    };
    ui.painter().rect_filled(rect, h / 2.0, fill);
    ui.painter().text(rect.center() - Vec2::new(0.0, 1.0), Align2::CENTER_CENTER, text, font, p.text);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A bare chevron (the sidebar mini-month's ‹ ›).
fn chevron_button(ui: &mut egui::Ui, text: &str, p: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, p.selection);
    }
    ui.painter().text(rect.center() - Vec2::new(0.0, 1.0), Align2::CENTER_CENTER, text, theme::font(18.0), p.secondary);
    resp
}

/// macOS-style segmented control.
fn segmented(ui: &mut egui::Ui, current: View, p: &Palette) -> Option<View> {
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, rect.height() / 2.0, p.seg_track);
    let w = rect.width() / View::ALL.len() as f32;
    let mut picked = None;
    for (i, v) in View::ALL.into_iter().enumerate() {
        let r = egui::Rect::from_min_size(rect.min + Vec2::new(i as f32 * w, 0.0), Vec2::new(w, rect.height()));
        let resp = ui.interact(r, ui.id().with(("seg", i)), Sense::click());
        if v == current {
            let knob = r.shrink(3.0);
            ui.painter().rect_filled(knob, knob.height() / 2.0, p.seg_knob);
        } else if i > 0 && View::ALL[i - 1] != current {
            ui.painter().vline(r.left(), r.y_range().shrink(9.0), Stroke::new(1.0_f32, p.grid));
        }
        ui.painter().text(r.center(), Align2::CENTER_CENTER, v.label(), theme::font(14.0), p.text);
        if resp.clicked() {
            picked = Some(v);
        }
    }
    picked
}

/// A sidebar group's heading: click to fold or unfold it, drag to move it.
/// A chevron at the right shows on hover (and while folded), as on macOS.
fn group_header(ui: &mut egui::Ui, key: &str, title: &str, collapsed: bool, p: &Palette) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    // Keyed by the group, not its place, so a drag survives reordering.
    let resp = ui.interact(rect, egui::Id::new(("sidebar-group", key)), Sense::click_and_drag());
    if resp.dragged() {
        ui.painter().rect_filled(rect.expand2(Vec2::new(0.0, 2.0)), 5.0, p.selection);
    }
    ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, title, theme::bold_font(11.5), p.secondary);
    if resp.hovered() || collapsed {
        let c = egui::pos2(rect.right() - 8.0, rect.center().y);
        let pts = if collapsed {
            [c + Vec2::new(-1.5, -3.5), c + Vec2::new(2.0, 0.0), c + Vec2::new(-1.5, 3.5)]
        } else {
            [c + Vec2::new(-3.5, -1.5), c + Vec2::new(0.0, 2.0), c + Vec2::new(3.5, -1.5)]
        };
        let s = Stroke::new(1.5_f32, p.secondary);
        ui.painter().line_segment([pts[0], pts[1]], s);
        ui.painter().line_segment([pts[1], pts[2]], s);
    }
    resp.on_hover_text(if collapsed { "Click to show, drag to move" } else { "Click to hide, drag to move" })
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
