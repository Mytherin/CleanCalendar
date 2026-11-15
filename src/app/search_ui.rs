//! The toolbar's search field (Ctrl+F) and the list of results beside the
//! calendar, like Apple Calendar's.

use chrono::{NaiveDate, Timelike};
use eframe::egui::{self, Align, Align2, Color32, Key, Rect, Sense, Stroke, Vec2};

use super::{App, View};
use crate::dates::fmt_time;
use crate::model::Event;
use crate::search::{self, Row};
use crate::theme::{self, Palette};

const FIELD_W: f32 = 220.0;
/// The results list: a day's heading band, one result, the gap that ends
/// each day's group, and the padding at the sides.
const DAY_H: f32 = 30.0;
const EVENT_H: f32 = 44.0;
const GROUP_GAP: f32 = 16.0;
const PAD: f32 = 12.0;

impl App {
    /// The magnifier button, or (once opened) the search field.
    pub(super) fn search_field(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let s = &mut self.search;
        if !s.open {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(28.0, 26.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 6.0, p.selection);
            }
            magnifier(ui.painter(), rect.center(), p.text);
            if resp.on_hover_text(format!("Search ({})", cmd_key!("F"))).clicked() {
                s.open = true;
                s.focus = true;
            }
            return;
        }
        let (rect, _) = ui.allocate_exact_size(Vec2::new(FIELD_W, 28.0), Sense::hover());
        ui.painter().rect_filled(rect, 7.0, p.seg_track);
        magnifier(ui.painter(), rect.left_center() + Vec2::new(14.0, 0.0), p.secondary);
        let text_rect = egui::Rect::from_min_max(rect.min + Vec2::new(26.0, 0.0), rect.max - Vec2::new(24.0, 0.0));
        let edit = egui::TextEdit::singleline(&mut s.query)
            .id(egui::Id::new("search-field"))
            .frame(false)
            .hint_text("Search")
            .vertical_align(Align::Center)
            .desired_width(text_rect.width());
        // In a child, so the toolbar's layout carries on left of the whole field.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).layout(egui::Layout::left_to_right(Align::Center)));
        let resp = child.add(edit);
        if s.focus {
            resp.request_focus();
            s.focus = false;
        }
        // The clear button.
        if !s.query.is_empty() {
            let c = rect.right_center() - Vec2::new(13.0, 0.0);
            let r = egui::Rect::from_center_size(c, Vec2::splat(16.0));
            let clear = ui.interact(r, ui.id().with("search-clear"), Sense::click());
            ui.painter().circle_filled(c, 7.0, if clear.hovered() { p.secondary } else { p.tertiary });
            let d = 2.6;
            let x = Stroke::new(1.4_f32, p.seg_track);
            ui.painter().line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], x);
            ui.painter().line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], x);
            if clear.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                s.query.clear();
                s.focus = true;
            }
        }
        if !resp.lost_focus() {
            return;
        }
        let (esc, enter) = ui.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
        if esc {
            // Like macOS: Esc clears the search and puts the field away.
            s.query.clear();
            s.open = false;
        } else if enter {
            // Go to the first thing coming up.
            let results = self.search.cache.get(&self.store, &self.search.query, self.today).to_vec();
            if let Some(i) = search::first_upcoming(&results, self.today) {
                self.show_result(&results[i]);
            }
        } else if s.query.trim().is_empty() && !s.focus {
            s.open = false;
        }
    }

    /// The results, in a panel on the right: grouped under a band for each
    /// day, like Apple Calendar, and scrolled to today's band.
    pub(super) fn search_results(&mut self, ctx: &egui::Context, p: &Palette) {
        let results = self.search.cache.get(&self.store, &self.search.query, self.today).to_vec();
        let rows = search::rows(&results, self.today);
        // Where each row starts; a gap ends each day's group.
        let mut tops = Vec::with_capacity(rows.len());
        let mut y = 0.0;
        for (i, &row) in rows.iter().enumerate() {
            if i > 0 && matches!(row, Row::Day(_)) {
                y += GROUP_GAP;
            }
            tops.push(y);
            y += row_height(row);
        }
        let total = y;
        let new_query = self.search.scrolled_for.as_deref() != Some(self.search.query.as_str());
        if new_query {
            self.search.scrolled_for = Some(self.search.query.clone());
        }
        let now = chrono::Local::now().naive_local();
        let mut clicked: Option<(usize, bool)> = None;
        egui::SidePanel::right("search-results")
            .resizable(false)
            .exact_width(300.0)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.bg))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0_f32, p.grid));
                if rows.is_empty() {
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, "No Results", theme::font(15.0), p.secondary);
                    return;
                }
                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if new_query {
                    let i = rows.iter().position(|&r| r == Row::Day(self.today)).unwrap_or(0);
                    area = area.vertical_scroll_offset(tops[i]);
                }
                area.show_viewport(ui, |ui, viewport| {
                    let origin = ui.cursor().min;
                    let width = ui.available_width();
                    ui.allocate_exact_size(Vec2::new(width, total), Sense::hover());
                    // Only the rows in sight are drawn: a weekly event alone
                    // can give a hundred results.
                    let first = tops.partition_point(|&t| t + EVENT_H < viewport.min.y);
                    for i in first..rows.len() {
                        if tops[i] > viewport.max.y {
                            break;
                        }
                        let rect = Rect::from_min_size(origin + Vec2::new(0.0, tops[i]), Vec2::new(width, row_height(rows[i])));
                        match rows[i] {
                            Row::Day(d) => day_band(ui, rect, d, d == self.today, p),
                            Row::Event(e) => {
                                let resp = self.result_row(ui, rect, &results[e], now, p);
                                if resp.double_clicked() {
                                    clicked = Some((e, true));
                                } else if resp.clicked() {
                                    clicked = Some((e, false));
                                }
                            }
                        }
                    }
                });
            });
        if let Some((i, open)) = clicked {
            self.show_result(&results[i]);
            if open {
                self.open_event(&results[i].id);
            }
        }
    }

    /// One result: the calendar's icon, the title (and location), and
    /// "all-day" or the times on the right. What's over is grayed out.
    fn result_row(&self, ui: &mut egui::Ui, rect: Rect, ev: &Event, now: chrono::NaiveDateTime, p: &Palette) -> egui::Response {
        let resp = ui.interact(rect, ui.id().with(("search-result", &ev.id)), Sense::click());
        let painter = ui.painter();
        let selected = self.selected.as_deref() == Some(ev.id.as_str());
        let hl = rect.shrink2(Vec2::new(4.0, 2.0));
        if selected {
            painter.rect_filled(hl, 6.0, p.selection);
        } else if resp.hovered() {
            painter.rect_filled(hl, 6.0, theme::blend(p.bg, p.selection, 0.5));
        }
        painter.hline(rect.x_range().shrink(4.0), rect.bottom() - 0.5, Stroke::new(1.0_f32, p.grid));

        let color = self.store.calendar(&ev.calendar_id).map_or(Color32::GRAY, |c| theme::rgb(c.color));
        let mid = rect.center().y;
        calendar_icon(painter, egui::pos2(rect.left() + PAD + 9.0, mid), color);
        let text = if ev.end <= now { p.secondary } else { p.text };

        // The right side first, so the title knows how much room it has.
        let right = rect.right() - PAD;
        let small = theme::font(12.0);
        let times_w = if ev.all_day {
            painter.text(egui::pos2(right, mid), Align2::RIGHT_CENTER, "all-day", small.clone(), p.secondary).width()
        } else {
            let a = painter.text(egui::pos2(right, mid - 8.0), Align2::RIGHT_CENTER, fmt_time(ev.start.time()), small.clone(), p.secondary);
            let b = painter.text(egui::pos2(right, mid + 8.0), Align2::RIGHT_CENTER, fmt_time(ev.end.time()), small.clone(), p.secondary);
            a.width().max(b.width())
        };
        let x = rect.left() + PAD + 26.0;
        let w = right - times_w - 10.0 - x;
        let title = if ev.title.trim().is_empty() { "New Event" } else { ev.title.as_str() };
        let title_font = theme::bold_font(14.0);
        let location = ev.location.trim();
        let title_y = if location.is_empty() { mid } else { mid - 8.0 };
        painter.text(egui::pos2(x, title_y), Align2::LEFT_CENTER, truncate(ui, title, title_font.clone(), w), title_font, text);
        if !location.is_empty() {
            painter.text(egui::pos2(x, mid + 9.0), Align2::LEFT_CENTER, truncate(ui, location, small.clone(), w), small, p.secondary);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Shows a result in the calendar and selects it. The year view has no
    /// events to select, so it goes to the day.
    fn show_result(&mut self, ev: &Event) {
        let view = if self.view == View::Year { View::Day } else { self.view };
        self.go_to(ev.start.date(), view);
        if matches!(view, View::Day | View::Week) && !ev.is_banner() {
            self.scroll_to = Some((ev.start.hour() as f32 - 1.0).max(0.0));
        }
        if self.editor.as_ref().is_none_or(|ed| ed.draft.id != ev.id) {
            self.finish_editor(crate::editor::Outcome::Done);
        }
        self.selected = Some(ev.id.clone());
    }
}

/// `text` cut to fit `width`, with an ellipsis.
fn truncate(ui: &egui::Ui, text: &str, font: egui::FontId, width: f32) -> String {
    let fits = |s: &str| ui.painter().layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x <= width;
    if fits(text) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if fits(&s) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_owned() + "…"
}

/// A magnifying glass: a ring and a handle.
fn magnifier(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let ring = center - Vec2::new(1.5, 1.5);
    let stroke = Stroke::new(1.6_f32, color);
    painter.circle_stroke(ring, 5.0, stroke);
    let d = 5.0 / std::f32::consts::SQRT_2;
    painter.line_segment([ring + Vec2::new(d, d), ring + Vec2::new(d + 4.0, d + 4.0)], Stroke::new(2.0_f32, color));
}

fn row_height(row: Row) -> f32 {
    match row {
        Row::Day(_) => DAY_H,
        Row::Event(_) => EVENT_H,
    }
}

/// A day's heading: a full-width band, "SUNDAY, 13 APRIL 2025", or a red
/// "TODAY".
fn day_band(ui: &egui::Ui, rect: Rect, day: NaiveDate, today: bool, p: &Palette) {
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, p.sidebar);
    let (label, color) = if today { ("TODAY".to_owned(), theme::RED) } else { (day.format("%A, %-d %B %Y").to_string().to_uppercase(), p.text) };
    painter.text(egui::pos2(rect.left() + PAD, rect.center().y), Align2::LEFT_CENTER, label, theme::bold_font(12.5), color);
}

/// A small calendar page in a circle of the calendar's color.
fn calendar_icon(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    painter.circle_filled(center, 9.0, color);
    let page = Rect::from_center_size(center, Vec2::new(10.0, 9.0));
    painter.rect_filled(page, 1.5, theme::blend(color, Color32::BLACK, 0.6));
    // The days: two rows of three.
    for row in 0..2 {
        for col in 0..3 {
            let c = egui::pos2(page.left() + 2.5 + col as f32 * 2.5, page.top() + 4.0 + row as f32 * 2.5);
            painter.rect_filled(Rect::from_center_size(c, Vec2::splat(1.4)), 0.0, color);
        }
    }
}
