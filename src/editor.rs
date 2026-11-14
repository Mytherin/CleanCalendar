//! The event editor, styled after macOS Calendar: grouped cards for the title
//! (with a calendar picker) and location, a one-line date summary that
//! expands into date/time fields, and notes. The same form is shown in a
//! popover next to the event, or docked in the day view's inspector.
//!
//! Like macOS, there are no Save/Cancel buttons: clicking outside the popover
//! (or moving on to another event in the inspector) keeps the changes, and
//! Esc discards them.

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use eframe::egui::{self, Align, Align2, Color32, Key, Layout, Order, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use crate::dates::*;
use crate::model::{Event, Store};
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
    Delete,
}

pub struct Editor {
    /// The event being edited. Its start/end follow the date/time fields as
    /// they change, so the calendar can preview it live.
    pub draft: Event,
    pub is_new: bool,
    pub placement: Placement,
    start_date: NaiveDate,
    start_time: NaiveTime,
    end_date: NaiveDate,
    end_time: NaiveTime,
    /// The date section is expanded into editable fields.
    editing_when: bool,
    error: Option<String>,
    /// The popover's size last frame, to place it before it's measured.
    size: Vec2,
    focus_title: bool,
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
            editing_when: false,
            error: None,
            size: Vec2::new(POPOVER_W, 260.0),
            focus_title: is_new,
        };
        ed.fields_from_draft();
        ed
    }

    fn fields_from_draft(&mut self) {
        let d = &self.draft;
        let last = if d.all_day { d.end - Duration::days(1) } else { d.end };
        self.start_date = d.start.date();
        self.start_time = d.start.time();
        self.end_date = last.date().max(d.start.date());
        self.end_time = d.end.time();
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
            ev.start = self.start_date.and_time(self.start_time);
            ev.end = self.end_date.and_time(self.end_time);
        }
        if ev.end < ev.start {
            return Err("The event ends before it starts.".into());
        }
        if ev.title.trim().is_empty() {
            ev.title = "New Event".into();
        }
        Ok(ev)
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
    // and the hovered row filled with that calendar's color.
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        v.menu_rounding = egui::Rounding::same(10.0);
        v.window_fill = p.popover;
        v.window_stroke = Stroke::new(1.0_f32, p.grid_strong);
        ui.style_mut().spacing.menu_margin = egui::Margin::symmetric(5.0, 6.0);
        egui::popup::popup_below_widget(ui, popup_id, &resp, egui::PopupCloseBehavior::CloseOnClick, |ui| {
            calendar_menu(ui, ed, store, p);
        });
    });
}

fn calendar_menu(ui: &mut egui::Ui, ed: &mut Editor, store: &Store, p: &Palette) {
    const ROW_H: f32 = 22.0;
    const PAD: f32 = 8.0;
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    // Wide enough for the longest name: check and dot columns, then the text.
    let text_w = store
        .calendars
        .iter()
        .map(|c| ui.fonts(|f| f.layout_no_wrap(c.name.clone(), theme::font(14.0), p.text).size().x))
        .fold(0.0, f32::max);
    let width = (PAD + 36.0 + text_w + PAD + 6.0).max(180.0);
    ui.set_width(width);

    let google_header = store.google.account_email.clone().unwrap_or_else(|| "Google".into());
    let groups = [(false, "On My Computer".to_owned()), (true, google_header)];
    let mut first = true;
    for (google, header) in groups {
        let cals: Vec<_> = store.calendars.iter().filter(|c| !c.read_only && c.google_id().is_some() == google).collect();
        if cals.is_empty() {
            continue;
        }
        if !first {
            let (r, _) = ui.allocate_exact_size(Vec2::new(width, 11.0), Sense::hover());
            ui.painter().hline(r.left() + PAD..=r.right() - PAD, r.center().y, Stroke::new(1.0_f32, p.grid_strong));
        }
        first = false;
        let (r, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H - 2.0), Sense::hover());
        let at = r.left_center() + Vec2::new(PAD, 0.0);
        ui.painter().text(at, Align2::LEFT_CENTER, header, theme::bold_font(12.0), p.secondary);

        for c in cals {
            let (r, row) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
            let color = theme::rgb(c.color);
            let (mut dot, mut text) = (color, p.text);
            if row.hovered() {
                ui.painter().rect_filled(r, 5.0, color);
                // Keep the dot visible on its own color, and the name legible.
                dot = color.gamma_multiply(0.6).to_opaque();
                let [cr, cg, cb, _] = color.to_array();
                let lum = 0.299 * cr as f32 + 0.587 * cg as f32 + 0.114 * cb as f32;
                text = if lum > 170.0 { Color32::from_gray(20) } else { Color32::WHITE };
            }
            if c.id == ed.draft.calendar_id {
                // A "✓", drawn (the UI font may not have the glyph).
                let c = Pos2::new(r.left() + PAD + 5.0, r.center().y);
                let s = Stroke::new(1.6_f32, text);
                ui.painter().line_segment([c + Vec2::new(-4.0, 0.0), c + Vec2::new(-1.0, 3.5)], s);
                ui.painter().line_segment([c + Vec2::new(-1.0, 3.5), c + Vec2::new(4.5, -4.5)], s);
            }
            ui.painter().circle_filled(Pos2::new(r.left() + PAD + 22.0, r.center().y), 6.0, dot);
            let at = r.left_center() + Vec2::new(PAD + 36.0, 0.0);
            ui.painter().text(at, Align2::LEFT_CENTER, &c.name, theme::font(14.0), text);
            if row.clicked() {
                ed.draft.calendar_id = c.id.clone();
            }
        }
    }
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
                        ui.push_id(id, |ui| ui.add_sized(DATE_BUTTON, egui_extras::DatePickerButton::new(date)));
                        if !ed.draft.all_day {
                            time_button(ui, &format!("{id}-time"), time);
                        }
                    });
                    ui.end_row();
                }
            });
        }
        if let Some(err) = &ed.error {
            ui.label(RichText::new(err).color(RED).small());
        }
    });

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

    ui.horizontal(|ui| {
        if read_only {
            ui.label(RichText::new("This calendar is read-only.").color(p.secondary).small());
        } else if !ed.is_new {
            let del = ui.add(egui::Label::new(RichText::new("Delete Event").color(RED)).sense(Sense::click()));
            if del.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                outcome = Outcome::Delete;
            }
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
