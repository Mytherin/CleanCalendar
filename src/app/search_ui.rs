//! The toolbar's search field (Ctrl+F) and the list of results beside the
//! calendar, like Apple Calendar's.

use chrono::Timelike;
use eframe::egui::{self, Align, Align2, Color32, Key, Sense, Stroke, Vec2};

use super::{App, View};
use crate::dates::fmt_time_range;
use crate::model::Event;
use crate::search;
use crate::theme::{self, Palette};

const FIELD_W: f32 = 220.0;
const ROW_H: f32 = 46.0;

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
            if resp.on_hover_text("Search (Ctrl+F)").clicked() {
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
            let results = search::search(&self.store, &self.search.query, self.today);
            if let Some(i) = search::first_upcoming(&results, self.today) {
                self.show_result(&results[i]);
            }
        } else if s.query.trim().is_empty() && !s.focus {
            s.open = false;
        }
    }

    /// The results, in a panel on the right, scrolled to what's coming up.
    pub(super) fn search_results(&mut self, ctx: &egui::Context, p: &Palette) {
        let results = search::search(&self.store, &self.search.query, self.today);
        let new_query = self.search.scrolled_for.as_deref() != Some(self.search.query.as_str());
        if new_query {
            self.search.scrolled_for = Some(self.search.query.clone());
        }
        let mut clicked: Option<(usize, bool)> = None;
        egui::SidePanel::right("search-results")
            .resizable(false)
            .exact_width(300.0)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.bg).inner_margin(egui::Margin { left: 10.0, right: 10.0, top: 8.0, bottom: 8.0 }))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().vline(r.left() - 10.0, r.y_range().expand(20.0), Stroke::new(1.0_f32, p.grid));
                let n = results.len();
                let count = match n {
                    0 => "No Results".to_owned(),
                    1 => "1 event".to_owned(),
                    n => format!("{n} events"),
                };
                ui.label(egui::RichText::new(count).font(theme::bold_font(12.0)).color(p.secondary));
                ui.add_space(4.0);
                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if new_query {
                    let i = search::first_upcoming(&results, self.today).unwrap_or(0);
                    area = area.vertical_scroll_offset(i as f32 * (ROW_H + ui.spacing().item_spacing.y));
                }
                area.show_rows(ui, ROW_H, n, |ui, range| {
                    for i in range {
                        let resp = self.result_row(ui, &results[i], p);
                        if resp.double_clicked() {
                            clicked = Some((i, true));
                        } else if resp.clicked() {
                            clicked = Some((i, false));
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

    fn result_row(&self, ui: &mut egui::Ui, ev: &Event, p: &Palette) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
        let selected = self.selected.as_deref() == Some(ev.id.as_str());
        if selected {
            ui.painter().rect_filled(rect, 6.0, p.selection);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, theme::blend(p.bg, p.selection, 0.5));
        }
        let color = self.store.calendar(&ev.calendar_id).map_or(Color32::GRAY, |c| theme::rgb(c.color));
        let top = rect.top() + 14.0;
        ui.painter().circle_filled(egui::pos2(rect.left() + 12.0, top), 4.5, color);
        let x = rect.left() + 24.0;
        let w = rect.right() - x - 8.0;
        let title = if ev.title.trim().is_empty() { "New Event" } else { ev.title.as_str() };
        let title = truncate(ui, title, theme::bold_font(13.5), w);
        ui.painter().text(egui::pos2(x, top), Align2::LEFT_CENTER, title, theme::bold_font(13.5), p.text);
        let when = if ev.all_day {
            let last = crate::dates::last_day(ev.start, ev.end);
            if last > ev.start.date() {
                format!("{} – {}", ev.start.format("%a %-d %b %Y"), last.format("%a %-d %b %Y"))
            } else {
                format!("{}  ·  All day", ev.start.format("%a %-d %b %Y"))
            }
        } else {
            format!("{}  ·  {}", ev.start.format("%a %-d %b %Y"), fmt_time_range(ev.start.time(), ev.end.time()))
        };
        let mut sub = when;
        if !ev.location.trim().is_empty() {
            sub = format!("{sub}  ·  {}", ev.location.trim());
        }
        let sub = truncate(ui, &sub, theme::font(12.0), w);
        ui.painter().text(egui::pos2(x, top + 18.0), Align2::LEFT_CENTER, sub, theme::font(12.0), p.secondary);
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
