//! The right-click menu, styled after macOS Calendar's: a rounded panel of
//! items with icons, separators between groups, a "Calendar ›" submenu,
//! keyboard shortcuts on the right and unavailable items greyed out.
//!
//! The app decides which items to show and what they do; this module only
//! draws the menu and reports what was picked.

use chrono::{NaiveDate, NaiveDateTime};
use eframe::egui::{self, Align2, Color32, Key, Order, PointerButton, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::model::{Event, Store};
use crate::theme::{self, Palette, MENU_HIGHLIGHT};

/// What was right-clicked.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAt {
    /// An event (by the id it's shown under).
    Event(String),
    /// Empty space in the hour grid, at this (snapped) time.
    Time(NaiveDateTime),
    /// An empty day: a month cell, or the all-day lane.
    Day { day: NaiveDate, all_day: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    GetInfo,
    /// Move the event to this calendar.
    MoveTo(String),
    Delete,
    Cut,
    Copy,
    Paste,
    Duplicate,
    NextOccurrence,
    PreviousOccurrence,
    EmailEvent,
    EmailSeries,
    NewEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Info,
    Calendar,
    Trash,
    Scissors,
    Copy,
    Paste,
    Duplicate,
    Next,
    Previous,
    Envelope,
    Envelopes,
    EnvelopePerson,
    Bubble,
    Plus,
}

pub enum Item {
    Command { icon: Icon, label: &'static str, shortcut: Option<&'static str>, enabled: bool, command: Command },
    /// "Calendar ›": a submenu of the calendars the event can move to.
    Calendars { enabled: bool, current: String },
    Separator,
}

impl Item {
    pub fn new(icon: Icon, label: &'static str, enabled: bool, command: Command) -> Item {
        Item::Command { icon, label, shortcut: None, enabled, command }
    }

    pub fn shortcut(mut self, keys: &'static str) -> Item {
        if let Item::Command { shortcut, .. } = &mut self {
            *shortcut = Some(keys);
        }
        self
    }
}

/// An open menu.
pub struct ContextMenu {
    pub at: MenuAt,
    /// Where the right-click happened.
    origin: Pos2,
    /// The menu's size last frame, to place it before it's measured.
    size: Vec2,
    sub_size: Vec2,
    submenu: bool,
    /// Opened this frame: the press that opened it doesn't close it.
    fresh: bool,
    /// The pointer moved since the menu opened, so releasing the right
    /// button over an item picks it (press, drag to an item, release).
    moved: bool,
}

impl ContextMenu {
    pub fn new(at: MenuAt, origin: Pos2) -> Self {
        ContextMenu {
            at,
            origin,
            size: Vec2::new(240.0, 200.0),
            sub_size: Vec2::new(200.0, 100.0),
            submenu: false,
            fresh: true,
            moved: false,
        }
    }
}

pub enum Outcome {
    Open,
    Close,
    Run(Command),
}

const ROW_H: f32 = 24.0;
const MARGIN: f32 = 5.0;
const ICON_X: f32 = 17.0;
const TEXT_X: f32 = 34.0;
const MENU_FONT: f32 = 14.0;

fn frame(p: &Palette) -> egui::Frame {
    let border = if p.dark { Color32::from_rgb(0x44, 0x47, 0x4A) } else { p.grid_strong };
    egui::Frame::none()
        .fill(p.popover)
        .rounding(10.0)
        .stroke(Stroke::new(1.0_f32, border))
        .shadow(egui::epaint::Shadow { offset: Vec2::new(0.0, 6.0), blur: 22.0, spread: 0.0, color: Color32::from_black_alpha(90) })
        .inner_margin(egui::Margin::same(MARGIN))
}

/// Draws the menu and reports what happened this frame.
pub fn show(ctx: &egui::Context, menu: &mut ContextMenu, items: &[Item], store: &Store, p: &Palette) -> Outcome {
    let screen = ctx.screen_rect();
    let (latest, released, presses, esc) = ctx.input(|i| {
        let presses: Vec<Pos2> = i
            .events
            .iter()
            .filter_map(|e| match e {
                egui::Event::PointerButton { pos, pressed: true, .. } => Some(*pos),
                _ => None,
            })
            .collect();
        (i.pointer.latest_pos(), i.pointer.button_released(PointerButton::Secondary), presses, i.key_pressed(Key::Escape))
    });
    if latest.is_some_and(|pos| pos.distance(menu.origin) > 4.0) {
        menu.moved = true;
    }
    let pick = released && menu.moved;

    // Below and right of the pointer; flipped above or left of it where it
    // would run off the window, so the pointer stays at a corner and is
    // never over an item when the menu opens.
    let size = menu.size;
    let mut pos = menu.origin + Vec2::splat(1.0);
    if pos.x + size.x > screen.right() - 4.0 {
        pos.x = menu.origin.x - size.x - 1.0;
    }
    if pos.y + size.y > screen.bottom() - 4.0 {
        pos.y = menu.origin.y - size.y - 1.0;
    }
    let pos = Pos2::new(pos.x.max(screen.left() + 4.0), pos.y.max(screen.top() + 4.0));

    let mut outcome = Outcome::Open;
    let mut calendars_row: Option<Rect> = None;
    let mut hover_calendars = false;
    let mut hover_other = false;
    let area = egui::Area::new(egui::Id::new("context-menu")).order(Order::Foreground).fixed_pos(pos).constrain(false).show(ctx, |ui| {
        frame(p).show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let width = menu_width(ui, items, p);
            ui.set_width(width);
            for item in items {
                match item {
                    Item::Separator => {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(width, 11.0), Sense::hover());
                        ui.painter().hline(r.left() + 10.0..=r.right() - 10.0, r.center().y, Stroke::new(1.0_f32, p.grid_strong));
                    }
                    Item::Command { icon, label, shortcut, enabled, command } => {
                        let resp = row(ui, width, *icon, label, *shortcut, *enabled, false, false, p);
                        let over = resp.contains_pointer();
                        hover_other |= over;
                        if *enabled && (resp.clicked() || (pick && over)) {
                            outcome = Outcome::Run(command.clone());
                        }
                    }
                    Item::Calendars { enabled, .. } => {
                        let open = *enabled && menu.submenu;
                        let resp = row(ui, width, Icon::Calendar, "Calendar", None, *enabled, open, true, p);
                        if resp.contains_pointer() {
                            if *enabled {
                                hover_calendars = true;
                            } else {
                                hover_other = true;
                            }
                        }
                        if *enabled && resp.clicked() {
                            menu.submenu = true;
                        }
                        calendars_row = Some(resp.rect);
                    }
                }
            }
        });
    });
    let main = area.response.rect;
    menu.size = main.size();

    // The submenu opens while "Calendar" is hovered, and stays open while
    // the pointer crosses over to it, until another item is hovered.
    if hover_calendars {
        menu.submenu = true;
    } else if hover_other {
        menu.submenu = false;
    }
    let mut sub_rect = None;
    let current = items.iter().find_map(|i| match i {
        Item::Calendars { enabled: true, current } => Some(current.as_str()),
        _ => None,
    });
    if let (true, Some(row), Some(current)) = (menu.submenu, calendars_row, current) {
        let mut x = main.right() - 2.0;
        if x + menu.sub_size.x > screen.right() - 4.0 {
            x = main.left() - menu.sub_size.x + 2.0;
        }
        let y = (row.top() - MARGIN - 1.0).min(screen.bottom() - menu.sub_size.y - 4.0).max(screen.top() + 4.0);
        let sub = egui::Area::new(egui::Id::new("context-submenu"))
            .order(Order::Foreground)
            .fixed_pos(Pos2::new(x.max(screen.left() + 4.0), y))
            .constrain(false)
            .show(ctx, |ui| {
                frame(p).show(ui, |ui| {
                    if let Some(id) = calendar_menu(ui, store, current, p) {
                        outcome = Outcome::Run(Command::MoveTo(id));
                    }
                });
            });
        ctx.move_to_top(sub.response.layer_id);
        menu.sub_size = sub.response.rect.size();
        sub_rect = Some(sub.response.rect);
    }

    if matches!(outcome, Outcome::Open) {
        let outside = |pos: &Pos2| !main.contains(*pos) && !sub_rect.is_some_and(|r| r.contains(*pos));
        if esc || (!menu.fresh && presses.iter().any(outside)) {
            outcome = Outcome::Close;
        }
    }
    menu.fresh = false;
    outcome
}

fn menu_width(ui: &egui::Ui, items: &[Item], p: &Palette) -> f32 {
    let w = |s: &str| ui.fonts(|f| f.layout_no_wrap(s.to_owned(), theme::font(MENU_FONT), p.text).size().x);
    items
        .iter()
        .map(|item| match item {
            Item::Command { label, shortcut, .. } => TEXT_X + w(label) + shortcut.map_or(0.0, |s| 28.0 + w(s)) + 16.0,
            Item::Calendars { .. } => TEXT_X + w("Calendar") + 40.0,
            Item::Separator => 0.0,
        })
        .fold(210.0, f32::max)
}

/// One item: highlighted while hovered (or while its submenu is open),
/// greyed out when it can't be used.
#[allow(clippy::too_many_arguments)]
fn row(
    ui: &mut egui::Ui,
    width: f32,
    icon: Icon,
    label: &str,
    shortcut: Option<&str>,
    enabled: bool,
    open: bool,
    chevron: bool,
    p: &Palette,
) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
    let lit = enabled && (resp.contains_pointer() || open);
    let painter = ui.painter();
    if lit {
        painter.rect_filled(r, 5.0, MENU_HIGHLIGHT);
    }
    let color = if !enabled {
        p.placeholder
    } else if lit {
        Color32::WHITE
    } else {
        p.text
    };
    draw_icon(painter, icon, Pos2::new(r.left() + ICON_X, r.center().y), color);
    painter.text(Pos2::new(r.left() + TEXT_X, r.center().y), Align2::LEFT_CENTER, label, theme::font(MENU_FONT), color);
    if let Some(keys) = shortcut {
        let c = if lit { Color32::WHITE } else if enabled { p.secondary } else { p.placeholder };
        painter.text(Pos2::new(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, keys, theme::font(MENU_FONT - 1.0), c);
    }
    if chevron {
        let c = Pos2::new(r.right() - 12.0, r.center().y);
        let s = Stroke::new(1.5_f32, color);
        painter.add(Shape::line(vec![c + Vec2::new(-2.0, -4.0), c + Vec2::new(2.0, 0.0), c + Vec2::new(-2.0, 4.0)], s));
    }
    resp
}

/// The menu's icons, drawn (the UI font has no glyphs for them), in a
/// 14×14 box around `c` in the style of SF Symbols' outlined icons.
fn draw_icon(painter: &egui::Painter, icon: Icon, c: Pos2, color: Color32) {
    let s = Stroke::new(1.25_f32, color);
    let v = |x: f32, y: f32| c + Vec2::new(x, y);
    let line = |pts: &[Pos2]| {
        painter.add(Shape::line(pts.to_vec(), s));
    };
    let rect = |a: Pos2, b: Pos2, r: f32| painter.rect_stroke(Rect::from_min_max(a, b), r, s);
    let pages = || {
        rect(v(-2.0, -3.0), v(5.0, 6.5), 1.5);
        line(&[v(-2.0, 3.5), v(-5.0, 3.5), v(-5.0, -6.5), v(2.0, -6.5), v(2.0, -3.0)]);
    };
    let envelope = |o: Vec2| {
        painter.rect_stroke(Rect::from_center_size(c + o, Vec2::new(13.0, 9.5)), 1.5, s);
        line(&[v(-6.5, -4.2) + o, v(0.0, 0.6) + o, v(6.5, -4.2) + o]);
    };
    match icon {
        Icon::Info => {
            painter.circle_stroke(c, 6.5, s);
            painter.circle_filled(v(0.0, -3.2), 0.95, color);
            line(&[v(0.0, -1.2), v(0.0, 3.6)]);
        }
        Icon::Calendar => {
            let r = Rect::from_center_size(c, Vec2::new(13.0, 12.0));
            painter.rect_stroke(r, 2.0, s);
            let band = Rect::from_min_max(r.min, Pos2::new(r.right(), r.top() + 3.5));
            painter.rect_filled(band, egui::Rounding { nw: 2.0, ne: 2.0, sw: 0.0, se: 0.0 }, color);
            for row in 0..2 {
                for col in 0..4 {
                    let at = Pos2::new(r.left() + 2.4 + col as f32 * 2.8, r.top() + 5.6 + row as f32 * 2.9);
                    painter.rect_filled(Rect::from_min_size(at, Vec2::splat(1.6)), 0.0, color);
                }
            }
        }
        Icon::Trash => {
            line(&[v(-6.0, -4.0), v(6.0, -4.0)]);
            line(&[v(-2.2, -4.0), v(-2.2, -6.2), v(2.2, -6.2), v(2.2, -4.0)]);
            line(&[v(-4.6, -4.0), v(-3.8, 6.5), v(3.8, 6.5), v(4.6, -4.0)]);
            line(&[v(-1.4, -1.5), v(-1.2, 4.0)]);
            line(&[v(1.4, -1.5), v(1.2, 4.0)]);
        }
        Icon::Scissors => {
            painter.circle_stroke(v(-4.2, -3.3), 2.1, s);
            painter.circle_stroke(v(-4.2, 3.3), 2.1, s);
            line(&[v(-2.4, -2.2), v(6.5, 3.8)]);
            line(&[v(-2.4, 2.2), v(6.5, -3.8)]);
        }
        Icon::Copy => pages(),
        Icon::Paste => {
            rect(v(-5.0, -4.5), v(5.0, 6.5), 1.5);
            painter.rect_filled(Rect::from_min_max(v(-2.5, -6.5), v(2.5, -3.2)), 1.0, color);
        }
        Icon::Duplicate => {
            pages();
            line(&[v(1.5, -0.5), v(1.5, 4.0)]);
            line(&[v(-0.75, 1.75), v(3.75, 1.75)]);
        }
        Icon::Next => {
            line(&[v(-5.5, 0.0), v(5.5, 0.0)]);
            line(&[v(1.5, -4.0), v(5.5, 0.0), v(1.5, 4.0)]);
        }
        Icon::Previous => {
            line(&[v(5.5, 0.0), v(-5.5, 0.0)]);
            line(&[v(-1.5, -4.0), v(-5.5, 0.0), v(-1.5, 4.0)]);
        }
        Icon::Envelope => envelope(Vec2::ZERO),
        Icon::Envelopes => {
            envelope(Vec2::new(0.0, 1.5));
            line(&[v(-5.0, -5.2), v(5.0, -5.2)]);
            line(&[v(-3.5, -7.0), v(3.5, -7.0)]);
        }
        Icon::EnvelopePerson => {
            envelope(Vec2::new(-1.0, -0.5));
            painter.circle_filled(v(5.2, 4.2), 2.6, color);
        }
        Icon::Bubble => {
            rect(v(-6.5, -5.5), v(6.5, 3.5), 4.5);
            line(&[v(-3.0, 3.5), v(-5.0, 6.5), v(0.5, 3.5)]);
        }
        Icon::Plus => {
            line(&[v(-5.5, 0.0), v(5.5, 0.0)]);
            line(&[v(0.0, -5.5), v(0.0, 5.5)]);
        }
    }
}

/// The writable calendars grouped under their account (as in the sidebar),
/// a check by `current`, the hovered row in the macOS menu highlight.
/// Returns the calendar picked. Shared by the editor's calendar picker and
/// the "Calendar ›" submenu.
pub fn calendar_menu(ui: &mut egui::Ui, store: &Store, current: &str, p: &Palette) -> Option<String> {
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
    let released = ui.input(|i| i.pointer.button_released(PointerButton::Secondary));

    let mut picked = None;
    let google_header = store.google.account_email.clone().unwrap_or_else(|| "Google".into());
    let groups = [(false, "On My Computer".to_owned()), (true, google_header)];
    let mut first = true;
    for (google, header) in groups {
        let cals: Vec<_> = store.calendars.iter().filter(|c| !c.read_only && store.is_listed(c) && c.google_id().is_some() == google).collect();
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
            let mut text = p.text;
            if row.contains_pointer() {
                ui.painter().rect_filled(r, 5.0, MENU_HIGHLIGHT);
                text = Color32::WHITE;
            }
            if c.id == current {
                draw_check(ui.painter(), Pos2::new(r.left() + PAD + 5.0, r.center().y), text);
            }
            ui.painter().circle_filled(Pos2::new(r.left() + PAD + 22.0, r.center().y), 6.0, theme::rgb(c.color));
            let at = r.left_center() + Vec2::new(PAD + 36.0, 0.0);
            ui.painter().text(at, Align2::LEFT_CENTER, &c.name, theme::font(14.0), text);
            if row.clicked() || (released && row.contains_pointer()) {
                picked = Some(c.id.clone());
            }
        }
    }
    picked
}

/// A "✓", drawn (the UI font may not have the glyph).
fn draw_check(painter: &egui::Painter, c: Pos2, color: Color32) {
    let s = Stroke::new(1.6_f32, color);
    painter.line_segment([c + Vec2::new(-4.0, 0.0), c + Vec2::new(-1.0, 3.5)], s);
    painter.line_segment([c + Vec2::new(-1.0, 3.5), c + Vec2::new(4.5, -4.5)], s);
}

// ---------------------------------------------------------------------------
// Sharing an event as text

/// The event as plain text (for e-mail and the clipboard): title, when,
/// location, how it repeats (`series` only), and notes.
pub fn event_text(ev: &Event, series: bool) -> String {
    let mut shown = ev.clone();
    if !series {
        shown.recurrence = None;
    }
    let mut s = crate::views::hover_text(&shown);
    if series && ev.recurrence.is_none() && ev.recurring_id.is_some() {
        s.push_str("\nRepeating event");
    }
    if !ev.notes.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(ev.notes.trim());
    }
    s
}

/// A `mailto:` link that opens a new message with this subject and body.
pub fn mailto(subject: &str, body: &str) -> String {
    format!("mailto:?subject={}&body={}", percent_encode(subject), percent_encode(&body.replace('\n', "\r\n")))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_encodes_everything_but_unreserved_characters() {
        assert_eq!(
            mailto("Lunch & café", "Fri 13\nRoom 1?"),
            "mailto:?subject=Lunch%20%26%20caf%C3%A9&body=Fri%2013%0D%0ARoom%201%3F"
        );
    }
}
