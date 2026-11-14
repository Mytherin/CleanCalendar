//! The event editor, styled after macOS Calendar: grouped cards for the title
//! (with a calendar picker) and location, a one-line date summary that
//! expands into date/time fields, and notes. The same form is shown in a
//! popover next to the event, or docked in the day view's inspector.
//!
//! Like macOS, there are no Save/Cancel buttons: clicking outside the popover
//! (or moving on to another event in the inspector) keeps the changes, and
//! Esc discards them.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use eframe::egui::{self, Align, Align2, Color32, Key, Layout, Order, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use crate::dates::*;
use crate::model::{looks_like_email, Attendee, Event, Response, Store};
use crate::recurrence::{self, Freq, Recurrence, RepeatEnd};
use crate::theme::{self, Palette, RED};

pub const POPOVER_W: f32 = 320.0;
const ARROW: f32 = 9.0;

/// Where the editor is shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Placement {
    /// Next to the event; `anchor` is where it was last drawn (or where the
    /// pointer was, for an event that isn't drawn yet).
    Popover { anchor: Option<Rect> },
    /// Docked in the day view's right-hand panel.
    Inspector,
}

/// What the user did with the editor this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Open,
    /// Keep the changes and close.
    Done,
    /// Discard the changes and close.
    Cancel,
    /// Delete the event, or just this occurrence of a repeating one.
    Delete,
    /// Delete every occurrence of the repeating event.
    DeleteSeries,
}

/// What a change to one occurrence of a repeating event applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    ThisEvent,
    AllEvents,
}

pub struct Editor {
    /// The event being edited. Its start/end follow the date/time fields as
    /// they change, so the calendar can preview it live.
    pub draft: Event,
    pub is_new: bool,
    pub placement: Placement,
    /// For an occurrence of a repeating event: what the changes apply to.
    pub scope: Scope,
    /// The date/time fields, in the event's time zone (if it has one).
    start_date: NaiveDate,
    start_time: NaiveTime,
    end_date: NaiveDate,
    end_time: NaiveTime,
    /// The date section is expanded into editable fields.
    editing_when: bool,
    error: Option<String>,
    /// What's typed in the "Add Invitees" field, not yet added.
    invitee_input: String,
    invitee_error: Option<String>,
    /// The popover's size last frame, to place it before it's measured.
    size: Vec2,
    focus_title: bool,
    /// The search text in the time zone picker.
    zone_filter: String,
}

impl Editor {
    pub fn new(draft: Event, is_new: bool, placement: Placement) -> Self {
        let mut ed = Editor {
            start_date: draft.start.date(),
            start_time: draft.start.time(),
            end_date: draft.start.date(),
            end_time: draft.end.time(),
            draft,
            is_new,
            placement,
            scope: Scope::AllEvents,
            editing_when: false,
            error: None,
            invitee_input: String::new(),
            invitee_error: None,
            size: Vec2::new(POPOVER_W, 260.0),
            focus_title: is_new,
            zone_filter: String::new(),
        };
        ed.fields_from_draft();
        ed
    }

    /// An occurrence of a repeating event stored here.
    fn is_occurrence(&self) -> bool {
        self.draft.recurrence.is_some() && recurrence::split_occurrence(&self.draft.id).is_some()
    }

    /// One of a series whose rule can't be edited here: Google sends its
    /// series as separate occurrences, and a changed occurrence is on its own.
    fn in_fixed_series(&self) -> bool {
        self.draft.recurrence.is_none() && self.draft.recurring_id.is_some()
    }

    fn fields_from_draft(&mut self) {
        let d = &self.draft;
        let zone = recurrence::zone_of(d);
        let (start, end) = (recurrence::to_zone(d.start, zone), recurrence::to_zone(d.end, zone));
        let last = if d.all_day { end - Duration::days(1) } else { end };
        self.start_date = start.date();
        self.start_time = start.time();
        self.end_date = last.date().max(start.date());
        self.end_time = end.time();
    }

    /// Updates the times from outside (e.g. the event was dragged while the
    /// popover is open).
    pub fn set_times(&mut self, start: NaiveDateTime, end: NaiveDateTime) {
        self.draft.start = start;
        self.draft.end = end;
        self.fields_from_draft();
    }

    /// The edited event, or why it can't be saved.
    pub fn result(&self) -> Result<Event, String> {
        let mut ev = self.draft.clone();
        if ev.all_day {
            ev.start = midnight(self.start_date);
            ev.end = midnight(self.end_date.max(self.start_date)) + Duration::days(1);
        } else {
            // The fields are in the event's zone; the event keeps local times.
            let zone = recurrence::zone_of(&ev);
            ev.start = recurrence::from_zone(self.start_date.and_time(self.start_time), zone);
            ev.end = recurrence::from_zone(self.end_date.and_time(self.end_time), zone);
        }
        if ev.end < ev.start {
            return Err("The event ends before it starts.".into());
        }
        if ev.title.trim().is_empty() {
            ev.title = "New Event".into();
        }
        // An address typed but not yet confirmed with Enter still counts.
        let (typed, _) = split_addresses(&self.invitee_input);
        add_invitees(&mut ev.attendees, typed);
        Ok(ev)
    }

    /// Moves complete addresses from the input field into the guest list;
    /// anything that isn't an address stays in the field.
    fn take_invitees(&mut self) {
        let (good, bad) = split_addresses(&self.invitee_input);
        add_invitees(&mut self.draft.attendees, good);
        self.invitee_error = (!bad.is_empty()).then(|| format!("\u{201C}{bad}\u{201D} isn't an email address."));
        self.invitee_input = bad;
    }

    /// Keeps the draft's times in step with the fields, for the live preview.
    fn sync_draft(&mut self) {
        match self.result() {
            Ok(ev) => {
                self.draft.start = ev.start;
                self.draft.end = ev.end;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }
}

/// Splits typed text at commas, semicolons, and spaces into email
/// addresses and whatever else is left (rejoined with ", ").
fn split_addresses(text: &str) -> (Vec<String>, String) {
    let (good, bad): (Vec<&str>, Vec<&str>) = text
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .partition(|s| looks_like_email(s));
    (good.into_iter().map(str::to_owned).collect(), bad.join(", "))
}

/// Adds addresses to a guest list, skipping ones already on it.
fn add_invitees(list: &mut Vec<Attendee>, emails: Vec<String>) {
    for email in emails {
        if !list.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
            list.push(Attendee::new(&email));
        }
    }
}

/// Whether the edited event differs from what's stored (so saving it would
/// change anything).
pub fn changed(ed: &Editor, stored: Option<&Event>) -> bool {
    let Ok(ev) = ed.result() else { return true };
    match stored {
        None => true,
        Some(s) => ev.fields() != s.fields() || ev.calendar_id != s.calendar_id,
    }
}

// ---------------------------------------------------------------------------
// The form

fn card<R>(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(p.card)
        .rounding(10.0)
        .inner_margin(egui::Margin::symmetric(12.0, 9.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn divider(ui: &mut egui::Ui, p: &Palette) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top() + 3.0, Stroke::new(1.0_f32, p.grid));
    ui.add_space(7.0);
}

/// "Friday, 13 November 2026" and "10:15 – 11:15" (or "all-day"), or a range
/// across days.
fn when_summary(ed: &Editor) -> (String, String) {
    let long = |d: NaiveDate| d.format("%A, %-d %B %Y").to_string();
    let short = |d: NaiveDate| d.format("%-d %b %Y").to_string();
    let (s, e) = (ed.start_date, ed.end_date.max(ed.start_date));
    match (ed.draft.all_day, s == e) {
        (true, true) => (long(s), "all-day".into()),
        (true, false) => (format!("{} \u{2013} {}", short(s), short(e)), String::new()),
        (false, true) => (long(s), format!("{} \u{2013} {}", fmt_time(ed.start_time), fmt_time(ed.end_time))),
        (false, false) => (
            format!("{} {} \u{2013} {} {}", short(s), fmt_time(ed.start_time), short(e), fmt_time(ed.end_time)),
            String::new(),
        ),
    }
}

/// Lines under the date summary: the time zone (with the local time, when
/// that's different) and how the event repeats.
fn when_details(ed: &Editor) -> Vec<String> {
    let mut lines = Vec::new();
    if let (Some(zone), false) = (&ed.draft.time_zone, ed.draft.all_day) {
        let mut line = zone_label(zone);
        if let Ok(ev) = ed.result() {
            let shown = ed.start_date.and_time(ed.start_time);
            if ev.start != shown {
                let day = if ev.start.date() != shown.date() { format!("{} ", ev.start.format("%a %-d %b")) } else { String::new() };
                line += &format!(" \u{00B7} {day}{} here", fmt_time_range(ev.start.time(), ev.end.time()));
            }
        }
        lines.push(line);
    }
    if let Some(rule) = &ed.draft.recurrence {
        lines.push(rule.describe(ed.start_date));
    } else if ed.in_fixed_series() {
        lines.push("Part of a repeating series".into());
    }
    lines
}

/// Size of the date and time buttons in the "starts"/"ends" rows. Both are
/// fixed so the two rows' clocks line up with each other.
const DATE_BUTTON: Vec2 = Vec2::new(112.0, 26.0);
const TIME_BUTTON: Vec2 = Vec2::new(80.0, 26.0);

/// A button showing the time that drops down a list of 15-minute slots.
/// (A plain button rather than an egui ComboBox: ComboBox ignores the row's
/// vertical centering, so it sat out of line with the date button beside it.)
fn time_button(ui: &mut egui::Ui, id: &str, t: &mut NaiveTime) {
    let resp = ui.add_sized(TIME_BUTTON, egui::Button::new(fmt_time(*t)));
    let popup_id = ui.make_persistent_id(id);
    if resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    egui::popup::popup_below_widget(ui, popup_id, &resp, egui::PopupCloseBehavior::CloseOnClick, |ui| {
        ui.set_min_width(TIME_BUTTON.x);
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for q in 0..96 {
                let slot = NaiveTime::from_hms_opt(q / 4, (q % 4) * 15, 0).unwrap();
                ui.selectable_value(t, slot, fmt_time(slot));
            }
        });
    });
}

/// A button that drops down a menu, built like [`time_button`]. The menu
/// closes when one of its items is clicked.
fn menu_button(ui: &mut egui::Ui, id: &str, label: &str, size: Vec2, menu: impl FnOnce(&mut egui::Ui)) {
    let resp = ui.add_sized(size, egui::Button::new(label).truncate());
    let popup_id = ui.make_persistent_id(id);
    if resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    egui::popup::popup_below_widget(ui, popup_id, &resp, egui::PopupCloseBehavior::CloseOnClick, |ui| {
        ui.set_min_width(size.x);
        menu(ui);
    });
}

/// "America/New York" for "America/New_York".
fn zone_label(name: &str) -> String {
    name.replace('_', " ")
}

/// The time zone picker: "Floating" (the event keeps its wall time wherever
/// you are), this computer's zone, then every zone, with a search field.
fn zone_button(ui: &mut egui::Ui, ed: &mut Editor) {
    let label = ed.draft.time_zone.as_deref().map_or("Floating".into(), zone_label);
    let size = Vec2::new(DATE_BUTTON.x + TIME_BUTTON.x + 8.0, DATE_BUTTON.y);
    let resp = ui.add_sized(size, egui::Button::new(label).truncate());
    let popup_id = ui.make_persistent_id("zone-picker");
    let opened = resp.clicked();
    if opened {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
        ed.zone_filter.clear();
    }
    egui::popup::popup_below_widget(ui, popup_id, &resp, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
        ui.set_min_width(size.x.max(240.0));
        let search = ui.add(egui::TextEdit::singleline(&mut ed.zone_filter).hint_text("Search").desired_width(f32::INFINITY));
        if opened {
            search.request_focus();
        }
        let filter = ed.zone_filter.trim().to_lowercase().replace('_', " ");
        let matches = |name: &str| filter.is_empty() || name.to_lowercase().replace('_', " ").contains(&filter);
        let mut chosen: Option<Option<String>> = None;
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            if matches("floating") && ui.selectable_label(ed.draft.time_zone.is_none(), "Floating").clicked() {
                chosen = Some(None);
            }
            let local = recurrence::local_zone_name();
            if let Some(local) = local.as_deref().filter(|n| matches(n)) {
                let current = ed.draft.time_zone.as_deref() == Some(local);
                if ui.selectable_label(current, format!("{} (this computer)", zone_label(local))).clicked() {
                    chosen = Some(Some(local.to_owned()));
                }
                ui.separator();
            }
            for name in recurrence::zone_names().filter(|n| matches(n) && Some(*n) != local.as_deref()) {
                if ui.selectable_label(ed.draft.time_zone.as_deref() == Some(name), zone_label(name)).clicked() {
                    chosen = Some(Some(name.to_owned()));
                }
            }
        });
        if let Some(zone) = chosen {
            // The date and time fields stay as they are: "09:00" now means
            // 09:00 in the chosen zone.
            ed.draft.time_zone = zone;
            ui.memory_mut(|m| m.close_popup());
        }
    });
}

/// "Never", "Every Day", …, or "Custom" for a rule the presets don't cover.
fn repeat_label(rule: Option<&Recurrence>) -> &'static str {
    match rule {
        None => "Never",
        Some(r) if r.interval > 1 || !r.weekdays.is_empty() => "Custom",
        Some(r) => match r.freq {
            Freq::Daily => "Every Day",
            Freq::Weekly => "Every Week",
            Freq::Monthly => "Every Month",
            Freq::Yearly => "Every Year",
        },
    }
}

/// The repeat rows of the date grid: how often, on which weekdays, and
/// until when.
fn repeat_rows(ui: &mut egui::Ui, ed: &mut Editor, p: &Palette) {
    let wide = Vec2::new(DATE_BUTTON.x, DATE_BUTTON.y);
    ui.label(RichText::new("repeat").color(p.secondary));
    let current = ed.draft.recurrence.clone();
    menu_button(ui, "repeat-menu", repeat_label(current.as_ref()), wide, |ui| {
        if ui.selectable_label(current.is_none(), "Never").clicked() {
            ed.draft.recurrence = None;
        }
        for freq in Freq::ALL {
            let preset = Recurrence::new(freq);
            let on = current.as_ref().is_some_and(|r| r.rule_eq(&Recurrence { end: r.end, ..preset.clone() }));
            if ui.selectable_label(on, repeat_label(Some(&preset))).clicked() {
                // Keep when it ends, and occurrences already removed.
                let mut rule = preset;
                if let Some(old) = &current {
                    rule.end = old.end;
                    rule.exceptions = old.exceptions.clone();
                }
                ed.draft.recurrence = Some(rule);
            }
        }
    });
    ui.end_row();

    let first_day = ed.start_date;
    let Some(rule) = ed.draft.recurrence.as_mut() else { return };
    ui.label(RichText::new("every").color(p.secondary));
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(&mut rule.interval).range(1..=99));
        ui.label(rule.freq.unit(rule.interval));
    });
    ui.end_row();

    if rule.freq == Freq::Weekly {
        ui.label(RichText::new("on").color(p.secondary));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for i in 0..7 {
                let wd = column_weekday(i);
                let on = rule.weekdays.contains(&wd) || (rule.weekdays.is_empty() && wd == first_day.weekday());
                let chip = egui::Button::new(RichText::new(weekday_initial(i)).color(if on { Color32::WHITE } else { p.text }))
                    .fill(if on { theme::ACCENT } else { p.button })
                    .min_size(Vec2::new(22.0, 22.0))
                    .rounding(11.0);
                if ui.add(chip).on_hover_text(day_short(wd)).clicked() {
                    if rule.weekdays.is_empty() {
                        rule.weekdays.push(first_day.weekday());
                    }
                    if on {
                        rule.weekdays.retain(|&d| d != wd);
                    } else {
                        rule.weekdays.push(wd);
                    }
                    // Only the first day's weekday is the same as no choice.
                    if rule.weekdays == [first_day.weekday()] {
                        rule.weekdays.clear();
                    }
                }
            }
        });
        ui.end_row();
    }

    ui.label(RichText::new("end repeat").color(p.secondary));
    ui.horizontal(|ui| {
        let label = match rule.end {
            RepeatEnd::Never => "Never",
            RepeatEnd::Until(_) => "On Date",
            RepeatEnd::Count(_) => "After",
        };
        let end = &mut rule.end;
        menu_button(ui, "end-repeat-menu", label, Vec2::new(TIME_BUTTON.x, DATE_BUTTON.y), |ui| {
            if ui.selectable_label(*end == RepeatEnd::Never, "Never").clicked() {
                *end = RepeatEnd::Never;
            }
            if ui.selectable_label(matches!(end, RepeatEnd::Until(_)), "On Date").clicked() {
                *end = RepeatEnd::Until(add_months(first_day, 1));
            }
            if ui.selectable_label(matches!(end, RepeatEnd::Count(_)), "After").clicked() {
                *end = RepeatEnd::Count(10);
            }
        });
        match &mut rule.end {
            RepeatEnd::Never => {}
            RepeatEnd::Until(d) => {
                date_button(ui, "until-date", d, p);
            }
            RepeatEnd::Count(n) => {
                ui.add(egui::DragValue::new(n).range(1..=999));
                ui.label(if *n == 1 { "time" } else { "times" });
            }
        }
    });
    ui.end_row();
}

/// A button showing the date ("04/11/2026") that drops down a compact month
/// calendar, like macOS Calendar's: "Nov 2026" with ◀ ● ▶ to page months,
/// today in red, the chosen day on a soft rounded square. Clicking a day
/// picks it and closes; clicking elsewhere or Esc just closes.
fn date_button(ui: &mut egui::Ui, id: &str, date: &mut NaiveDate, p: &Palette) {
    let popup_id = ui.make_persistent_id(id);
    let month_id = popup_id.with("month");
    let open = ui.memory(|m| m.is_popup_open(popup_id));
    let resp = ui.add_sized(DATE_BUTTON, egui::Button::new(date.format("%d/%m/%Y").to_string()).selected(open));
    if resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
        ui.data_mut(|d| d.insert_temp(month_id, first_of_month(*date)));
    }
    if !ui.memory(|m| m.is_popup_open(popup_id)) {
        return;
    }

    const CELL: Vec2 = Vec2::new(27.0, 22.0);
    let today = chrono::Local::now().date_naive();
    let mut month = ui.data(|d| d.get_temp::<NaiveDate>(month_id)).unwrap_or_else(|| first_of_month(*date));
    let mut close = false;
    let border = if p.dark { Color32::from_rgb(0x4A, 0x4D, 0x50) } else { p.grid_strong };

    let area = egui::Area::new(popup_id)
        .order(Order::Foreground)
        .fixed_pos(resp.rect.left_bottom() + Vec2::new(0.0, 4.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::none()
                .fill(p.popover)
                .rounding(8.0)
                .stroke(Stroke::new(1.0_f32, border))
                .shadow(egui::epaint::Shadow { offset: Vec2::new(0.0, 4.0), blur: 14.0, spread: 0.0, color: Color32::from_black_alpha(70) })
                .inner_margin(egui::Margin::symmetric(6.0, 6.0))
                .show(ui, |ui| {
                    let w = 7.0 * CELL.x;
                    // Header: "Nov 2026" on the left, ◀ ● ▶ on the right.
                    let (head, _) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
                    ui.painter().text(
                        head.left_center() + Vec2::new(3.0, 0.0),
                        Align2::LEFT_CENTER,
                        month.format("%b %Y").to_string(),
                        theme::bold_font(15.0),
                        p.text,
                    );
                    let nav = |ui: &mut egui::Ui, k: i32| {
                        let c = Pos2::new(head.right() - 9.0 - (2 - k) as f32 * 16.0, head.center().y);
                        let r = Rect::from_center_size(c, Vec2::splat(16.0));
                        let resp = ui.interact(r, popup_id.with(("nav", k)), Sense::click());
                        let color = if resp.hovered() { p.text } else { p.secondary };
                        match k {
                            0 => ui.painter().add(egui::Shape::convex_polygon(
                                vec![c + Vec2::new(-4.0, 0.0), c + Vec2::new(3.0, -4.5), c + Vec2::new(3.0, 4.5)],
                                color,
                                Stroke::NONE,
                            )),
                            1 => ui.painter().add(egui::Shape::circle_filled(c, 3.5, color)),
                            _ => ui.painter().add(egui::Shape::convex_polygon(
                                vec![c + Vec2::new(4.0, 0.0), c + Vec2::new(-3.0, 4.5), c + Vec2::new(-3.0, -4.5)],
                                color,
                                Stroke::NONE,
                            )),
                        };
                        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
                    };
                    if nav(ui, 0) {
                        month = add_months(month, -1);
                    }
                    if nav(ui, 1) {
                        month = first_of_month(today);
                    }
                    if nav(ui, 2) {
                        month = add_months(month, 1);
                    }

                    // Weekday initials ("Mo", "Tu", …) over a hairline.
                    let (days, _) = ui.allocate_exact_size(Vec2::new(w, CELL.y), Sense::hover());
                    for i in 0..7 {
                        ui.painter().text(
                            Pos2::new(days.left() + (i as f32 + 0.5) * CELL.x, days.center().y),
                            Align2::CENTER_CENTER,
                            &weekday_short(i)[..2],
                            theme::bold_font(12.0),
                            p.secondary,
                        );
                    }
                    ui.painter().hline(days.x_range().shrink(3.0), days.bottom() + 1.0, Stroke::new(1.0_f32, p.grid));
                    ui.add_space(4.0);

                    // Six weeks of days.
                    let (grid, _) = ui.allocate_exact_size(Vec2::new(w, 6.0 * CELL.y), Sense::hover());
                    let start = start_of_week(month);
                    for k in 0..42 {
                        let day = start + Duration::days(k);
                        let r = Rect::from_min_size(
                            grid.min + Vec2::new((k % 7) as f32 * CELL.x, (k / 7) as f32 * CELL.y),
                            CELL,
                        );
                        let resp = ui.interact(r, popup_id.with(("day", k)), Sense::click());
                        let bg = r.shrink2(Vec2::new(1.5, 1.0));
                        if day == *date {
                            ui.painter().rect_filled(bg, 5.0, if p.dark { p.grid_strong } else { p.button });
                        } else if resp.hovered() {
                            ui.painter().rect_filled(bg, 5.0, p.selection);
                        }
                        let in_month = day.month() == month.month();
                        let color = if day == today {
                            RED
                        } else if in_month {
                            p.text
                        } else {
                            p.tertiary
                        };
                        let font = if day == *date || day == today { theme::bold_font(13.0) } else { theme::font(13.0) };
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, day.day().to_string(), font, color);
                        if resp.clicked() {
                            *date = day;
                            close = true;
                        }
                    }
                });
        });
    ui.data_mut(|d| d.insert_temp(month_id, month));

    // Close on a press outside the calendar (other than on the button, which
    // toggles it itself), or on Esc — swallowing the Esc so it doesn't also
    // discard the whole edit.
    let outside = ui.input(|i| {
        i.pointer.any_pressed()
            && i.pointer.press_origin().is_some_and(|pos| !area.response.rect.contains(pos) && !resp.rect.contains(pos))
    });
    let esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
    if close || outside || esc {
        ui.memory_mut(|m| m.close_popup());
    }
}

/// The calendar picker: a chip with the calendar's color dot; clicking it
/// lists the calendars the event can move to.
fn calendar_chip(ui: &mut egui::Ui, ed: &mut Editor, store: &Store, p: &Palette, read_only: bool) {
    let color = store.calendar(&ed.draft.calendar_id).map(|c| theme::rgb(c.color)).unwrap_or(p.secondary);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(40.0, 26.0), Sense::click());
    let hovered = resp.hovered() && !read_only;
    ui.painter().rect_filled(rect, 8.0, if hovered { p.button } else { p.selection });
    ui.painter().circle_filled(Pos2::new(rect.left() + 13.0, rect.center().y), 6.0, color);
    // A small "⌄" chevron, drawn (the UI font has no glyph for it).
    let c = Pos2::new(rect.right() - 11.0, rect.center().y);
    let s = Stroke::new(1.5_f32, p.secondary);
    ui.painter().line_segment([c + Vec2::new(-3.5, -1.5), c + Vec2::new(0.0, 2.0)], s);
    ui.painter().line_segment([c + Vec2::new(0.0, 2.0), c + Vec2::new(3.5, -1.5)], s);
    let resp = resp.on_hover_text(store.calendar(&ed.draft.calendar_id).map(|c| c.name.clone()).unwrap_or_default());
    if read_only {
        return;
    }
    let popup_id = ui.id().with("calendar-picker");
    if resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    // A macOS-style menu: a rounded, outlined panel with the calendars grouped
    // under their account (as in the sidebar), a check by the current one,
    // and the hovered row filled with the macOS menu highlight red.
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        v.menu_rounding = egui::Rounding::same(10.0);
        v.window_fill = p.popover;
        v.window_stroke = Stroke::new(1.0_f32, p.grid_strong);
        ui.style_mut().spacing.menu_margin = egui::Margin::symmetric(5.0, 6.0);
        egui::popup::popup_below_widget(ui, popup_id, &resp, egui::PopupCloseBehavior::CloseOnClick, |ui| {
            if let Some(id) = crate::menu::calendar_menu(ui, store, &ed.draft.calendar_id, p) {
                ed.draft.calendar_id = id;
            }
        });
    });
}

/// A small round badge for an invitee's reply, drawn like macOS: a hollow
/// ring while waiting, a green tick, an orange "?" or a red cross.
fn response_badge(ui: &mut egui::Ui, response: Response, p: &Palette) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    let white = Stroke::new(1.6_f32, Color32::WHITE);
    let tip = match response {
        Response::NeedsAction => {
            painter.circle_stroke(c, 6.0, Stroke::new(1.2_f32, p.tertiary));
            "No response yet"
        }
        Response::Accepted => {
            painter.circle_filled(c, 7.0, Color32::from_rgb(0x34, 0xC7, 0x59));
            painter.line_segment([c + Vec2::new(-3.0, 0.2), c + Vec2::new(-0.8, 2.5)], white);
            painter.line_segment([c + Vec2::new(-0.8, 2.5), c + Vec2::new(3.2, -2.5)], white);
            "Accepted"
        }
        Response::Tentative => {
            painter.circle_filled(c, 7.0, Color32::from_rgb(0xFF, 0x95, 0x00));
            painter.text(c, Align2::CENTER_CENTER, "?", theme::bold_font(10.0), Color32::WHITE);
            "Maybe"
        }
        Response::Declined => {
            painter.circle_filled(c, 7.0, RED);
            painter.line_segment([c + Vec2::new(-2.5, -2.5), c + Vec2::new(2.5, 2.5)], white);
            painter.line_segment([c + Vec2::new(-2.5, 2.5), c + Vec2::new(2.5, -2.5)], white);
            "Declined"
        }
    };
    resp.on_hover_text(tip);
}

/// The guest list and the "Add Invitees" field. Invitations go out through
/// Google, so on a local calendar the card only appears if the event already
/// has guests (e.g. it was moved off a Google calendar), with a note.
fn invitees_card(ui: &mut egui::Ui, ed: &mut Editor, store: &Store, p: &Palette, read_only: bool) {
    let on_google = store.calendar(&ed.draft.calendar_id).is_some_and(|c| c.google_id().is_some());
    let has_guests = ed.draft.attendees.iter().any(|a| !a.resource);
    if !has_guests && (read_only || !on_google) {
        return;
    }
    card(ui, p, |ui| {
        let mut remove = None;
        for (i, a) in ed.draft.attendees.iter().enumerate().filter(|(_, a)| !a.resource) {
            ui.horizontal(|ui| {
                response_badge(ui, a.response, p);
                let mut text = RichText::new(a.label()).color(p.text);
                if a.response == Response::Declined {
                    text = text.strikethrough();
                }
                let label = ui.label(text);
                if !a.name.is_empty() {
                    label.on_hover_text(&a.email);
                }
                let role = if a.organizer {
                    "organizer"
                } else if a.is_self {
                    "you"
                } else if a.optional {
                    "optional"
                } else {
                    ""
                };
                if !role.is_empty() {
                    ui.label(RichText::new(role).color(p.secondary).small());
                }
                if !read_only && !a.organizer {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let x = ui.add(egui::Label::new(RichText::new("\u{2715}").color(p.tertiary)).sense(Sense::click()));
                        if x.on_hover_text("Remove").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            remove = Some(i);
                        }
                    });
                }
            });
        }
        if let Some(i) = remove {
            ed.draft.attendees.remove(i);
        }
        if !read_only {
            if has_guests {
                divider(ui, p);
            }
            let field = ui.add(
                egui::TextEdit::singleline(&mut ed.invitee_input)
                    .id_source("invitee-input")
                    .hint_text(RichText::new("Add Invitees").color(p.placeholder))
                    .text_color(p.text)
                    .frame(false)
                    .desired_width(f32::INFINITY),
            );
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let separated = field.changed() && ed.invitee_input.ends_with([',', ';']);
            if entered || separated {
                ed.take_invitees();
                if entered {
                    // Stay in the field for the next address.
                    field.request_focus();
                }
            } else if field.changed() {
                ed.invitee_error = None;
            }
        }
        if let Some(err) = &ed.invitee_error {
            ui.label(RichText::new(err).color(RED).small());
        }
        if !on_google && has_guests {
            ui.label(RichText::new("Invitations are only sent for events on a Google calendar.").color(p.secondary).small());
        }
    });
}

/// Draws the editor's contents into `ui`.
pub fn form(ui: &mut egui::Ui, ed: &mut Editor, store: &Store, p: &Palette) -> Outcome {
    let read_only = store.calendar(&ed.draft.calendar_id).is_some_and(|c| c.read_only);
    let mut outcome = Outcome::Open;
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

    // Title + calendar, then location.
    card(ui, p, |ui| {
        ui.add_enabled_ui(!read_only, |ui| {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 48.0;
                let title = ui.add(
                    egui::TextEdit::singleline(&mut ed.draft.title)
                        .hint_text(RichText::new("New Event").color(p.placeholder))
                        .font(theme::bold_font(18.0))
                        .text_color(p.text)
                        .frame(false)
                        .desired_width(w),
                );
                if std::mem::take(&mut ed.focus_title) {
                    title.request_focus();
                }
                if title.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    outcome = Outcome::Done;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| calendar_chip(ui, ed, store, p, read_only));
            });
            divider(ui, p);
            ui.add(
                egui::TextEdit::singleline(&mut ed.draft.location)
                    .hint_text(RichText::new("Add Location").color(p.placeholder))
                    .text_color(p.text)
                    .frame(false)
                    .desired_width(f32::INFINITY),
            );
        });
    });

    // When: a summary line that expands into editable fields.
    card(ui, p, |ui| {
        let (left, right) = when_summary(ed);
        let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::click());
        ui.painter().text(row.left_center(), Align2::LEFT_CENTER, left, theme::font(14.0), p.text);
        ui.painter().text(row.right_center(), Align2::RIGHT_CENTER, right, theme::font(14.0), p.text);
        if !read_only {
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                ed.editing_when = !ed.editing_when;
            }
        }
        if ed.editing_when && !read_only {
            divider(ui, p);
            egui::Grid::new("when-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label(RichText::new("all-day").color(p.secondary));
                ui.checkbox(&mut ed.draft.all_day, "");
                ui.end_row();
                for (label, date, time, id) in [
                    ("starts", &mut ed.start_date, &mut ed.start_time, "start"),
                    ("ends", &mut ed.end_date, &mut ed.end_time, "end"),
                ] {
                    ui.label(RichText::new(label).color(p.secondary));
                    ui.horizontal(|ui| {
                        date_button(ui, &format!("{id}-date"), date, p);
                        if !ed.draft.all_day {
                            time_button(ui, &format!("{id}-time"), time);
                        }
                    });
                    ui.end_row();
                }
                if !ed.draft.all_day {
                    ui.label(RichText::new("time zone").color(p.secondary));
                    zone_button(ui, ed);
                    ui.end_row();
                }
                if !ed.in_fixed_series() {
                    repeat_rows(ui, ed, p);
                }
            });
        }
        for line in when_details(ed) {
            ui.label(RichText::new(line).color(p.secondary).small());
        }
        if let Some(err) = &ed.error {
            ui.label(RichText::new(err).color(RED).small());
        }
    });

    invitees_card(ui, ed, store, p, read_only);

    // Notes.
    card(ui, p, |ui| {
        ui.add_enabled_ui(!read_only, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut ed.draft.notes)
                    .hint_text(RichText::new("Add Notes or URL").color(p.placeholder))
                    .text_color(p.text)
                    .frame(false)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
        });
    });

    // Changing one occurrence of a repeating event: this one, or all?
    // (A new repeat rule always applies to the whole series.)
    if ed.is_occurrence() && !read_only {
        let stored = store.find_event(&ed.draft.id);
        let same_rule = match (&ed.draft.recurrence, stored.as_ref().and_then(|s| s.recurrence.as_ref())) {
            (Some(a), Some(b)) => a.rule_eq(b),
            _ => false,
        };
        if same_rule && changed(ed, stored.as_ref()) {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Apply to").color(p.secondary));
                ui.selectable_value(&mut ed.scope, Scope::ThisEvent, "This Event");
                ui.selectable_value(&mut ed.scope, Scope::AllEvents, "All Events");
            });
        }
    }

    ui.horizontal(|ui| {
        let link = |ui: &mut egui::Ui, text: &str| {
            let l = ui.add(egui::Label::new(RichText::new(text).color(RED)).sense(Sense::click()));
            l.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        };
        if read_only {
            ui.label(RichText::new("This calendar is read-only.").color(p.secondary).small());
        } else if ed.is_new {
        } else if ed.is_occurrence() || ed.in_fixed_series() {
            if link(ui, "Delete This Event") {
                outcome = Outcome::Delete;
            }
            ui.add_space(12.0);
            if link(ui, "Delete All Events") {
                outcome = Outcome::DeleteSeries;
            }
        } else if link(ui, "Delete Event") {
            outcome = Outcome::Delete;
        }
    });

    ed.sync_draft();
    outcome
}

// ---------------------------------------------------------------------------
// The popover

/// Shows the editor in a popover beside its event. Clicking anywhere outside
/// it keeps the changes; Esc discards them.
pub fn popover(ctx: &egui::Context, ed: &mut Editor, store: &Store, p: &Palette, anchor: Option<Rect>) -> Outcome {
    let screen = ctx.screen_rect();
    let size = ed.size;
    // Right of the event if it fits, else left; vertically near its top.
    let (pos, arrow_left) = match anchor {
        Some(a) => {
            let right = a.right() + ARROW + 4.0;
            let on_right = right + size.x <= screen.right() - 8.0;
            let x = if on_right { right } else { a.left() - ARROW - 4.0 - size.x };
            let y = (a.top() - 24.0).clamp(screen.top() + 8.0, (screen.bottom() - size.y - 8.0).max(screen.top() + 8.0));
            (Pos2::new(x.max(screen.left() + 8.0), y), Some((on_right, a.center().y.clamp(a.top() + 8.0, a.top() + 40.0))))
        }
        None => (screen.center() - size / 2.0, None),
    };

    let mut outcome = Outcome::Open;
    let area = egui::Area::new(egui::Id::new("event-popover"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .constrain(false)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(p.popover)
                .rounding(12.0)
                .stroke(Stroke::new(1.0_f32, if p.dark { Color32::from_rgb(0x34, 0x37, 0x3A) } else { p.grid_strong }))
                .shadow(egui::epaint::Shadow { offset: Vec2::new(0.0, 6.0), blur: 24.0, spread: 0.0, color: Color32::from_black_alpha(90) })
                .inner_margin(egui::Margin::same(12.0))
                .show(ui, |ui| {
                    ui.set_width(POPOVER_W - 24.0);
                    outcome = form(ui, ed, store, p);
                });
        });
    let rect = area.response.rect;
    ed.size = rect.size();

    // The arrow pointing at the event.
    if let Some((on_right, y)) = arrow_left {
        let y = y.clamp(rect.top() + 18.0, rect.bottom() - 18.0);
        let painter = ctx.layer_painter(area.response.layer_id);
        let (base, tip) = if on_right { (rect.left(), rect.left() - ARROW) } else { (rect.right(), rect.right() + ARROW) };
        let stroke = Stroke::new(1.0_f32, if p.dark { Color32::from_rgb(0x34, 0x37, 0x3A) } else { p.grid_strong });
        let pts = vec![Pos2::new(base, y - ARROW), Pos2::new(tip, y), Pos2::new(base, y + ARROW)];
        painter.add(egui::Shape::convex_polygon(pts.clone(), p.popover, Stroke::NONE));
        painter.line_segment([pts[0], pts[1]], stroke);
        painter.line_segment([pts[1], pts[2]], stroke);
    }

    if outcome == Outcome::Open {
        let (pressed, press_pos, esc) =
            ctx.input(|i| (i.pointer.any_pressed(), i.pointer.press_origin(), i.key_pressed(Key::Escape)));
        if esc {
            outcome = Outcome::Cancel;
        } else if pressed {
            // A press on the calendar itself (not on this popover or one of
            // its own pop-ups, such as the date picker) closes it.
            let on_background = press_pos
                .and_then(|pos| ctx.layer_id_at(pos))
                .map_or(true, |layer| layer.order == Order::Background || layer.order == Order::Middle);
            if on_background && !press_pos.is_some_and(|pos| rect.contains(pos)) {
                outcome = Outcome::Done;
            }
        }
    }
    outcome
}

/// The same form docked in the day view's right-hand panel.
pub fn inspector(ui: &mut egui::Ui, ed: &mut Editor, store: &Store, p: &Palette) -> Outcome {
    let outcome = form(ui, ed, store, p);
    if outcome == Outcome::Open && ui.input(|i| i.key_pressed(Key::Escape)) {
        return Outcome::Cancel;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let start = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let ev = Event {
            id: "local:1".into(),
            calendar_id: "google:primary".into(),
            title: "Lunch".into(),
            start,
            end: start + Duration::hours(1),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            attendees: vec![Attendee::new("ann@example.com")],
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
            recurrence: None,
            time_zone: None,
        };
        Editor::new(ev, true, Placement::Inspector)
    }

    fn emails(list: &[Attendee]) -> Vec<&str> {
        list.iter().map(|a| a.email.as_str()).collect()
    }

    #[test]
    fn typed_addresses_are_added_once_and_junk_stays_in_the_field() {
        let mut ed = editor();
        ed.invitee_input = "bob@example.com, oops; ANN@example.com carol@example.org".into();
        ed.take_invitees();
        assert_eq!(emails(&ed.draft.attendees), ["ann@example.com", "bob@example.com", "carol@example.org"]);
        assert_eq!(ed.invitee_input, "oops");
        assert!(ed.invitee_error.is_some());
    }

    #[test]
    fn an_address_left_in_the_field_is_saved() {
        let mut ed = editor();
        ed.invitee_input = "dave@example.com".into();
        let ev = ed.result().unwrap();
        assert_eq!(emails(&ev.attendees), ["ann@example.com", "dave@example.com"]);
        ed.invitee_input = "not an address".into();
        assert_eq!(emails(&ed.result().unwrap().attendees), ["ann@example.com"]);
    }
}
