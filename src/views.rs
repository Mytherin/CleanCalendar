//! The calendar views: month grid, day/week time grid, year overview, and
//! the sidebar mini-month.

use std::sync::Arc;

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime};
use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, CursorIcon, FontId, Galley, Pos2, Rect, Sense, Stroke, Ui, Vec2,
};

use crate::app::{Action, App, Drag, View};
use crate::dates::*;
use crate::model::Event;
use crate::theme::{self, Palette, RED};

pub const HOUR_H: f32 = 48.0;
const GUTTER: f32 = 58.0;
const BANNER_H: f32 = 20.0;

// ---------------------------------------------------------------------------
// Shared helpers

fn text_galley(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_w.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|f| f.layout_job(job))
}

fn draw_text(ui: &Ui, painter: &egui::Painter, pos: Pos2, text: &str, font: FontId, color: Color32, max_w: f32) -> f32 {
    let g = text_galley(ui, text, font, color, max_w);
    let w = g.size().x;
    painter.galley(pos, g, color);
    w
}

fn event_color(app: &App, ev: &Event) -> Color32 {
    app.store.calendar(&ev.calendar_id).map(|c| theme::rgb(c.color)).unwrap_or(theme::ACCENT)
}

fn is_read_only(app: &App, ev: &Event) -> bool {
    app.store.calendar(&ev.calendar_id).is_some_and(|c| c.read_only)
}

/// Visible events overlapping [first, first + n days).
fn events_in(app: &App, first: NaiveDate, n: i64) -> Vec<Event> {
    let a = midnight(first);
    let b = midnight(first + Duration::days(n));
    let mut v: Vec<Event> = app
        .store
        .events
        .iter()
        .filter(|e| app.store.is_visible(&e.calendar_id))
        .filter(|e| e.start < b && (e.end > a || (e.end <= e.start && e.start >= a)))
        .cloned()
        .collect();
    v.sort_by(|x, y| x.start.cmp(&y.start).then(y.duration().cmp(&x.duration())));
    v
}

/// Packs banner (all-day / multi-day) events into lanes across a run of days.
/// Returns (event index, first column, last column, lane).
fn banner_lanes(events: &[Event], first: NaiveDate, ndays: i64) -> Vec<(usize, i64, i64, usize)> {
    let mut lanes: Vec<i64> = Vec::new(); // last occupied column per lane
    let mut out = Vec::new();
    for (i, ev) in events.iter().enumerate().filter(|(_, e)| e.is_banner()) {
        let c0 = (ev.start.date() - first).num_days().max(0);
        let c1 = (last_day(ev.start, ev.end) - first).num_days().min(ndays - 1);
        if c1 < 0 || c0 >= ndays {
            continue;
        }
        let lane = match lanes.iter().position(|&end| end < c0) {
            Some(l) => l,
            None => {
                lanes.push(-1);
                lanes.len() - 1
            }
        };
        lanes[lane] = c1;
        out.push((i, c0, c1, lane));
    }
    out
}

fn day_number_badge(painter: &egui::Painter, center: Pos2, text: &str, is_today: bool, font: FontId, color: Color32) {
    if is_today {
        let w = (text.len() as f32 * font.size * 0.32).max(font.size * 0.62);
        painter.rect_filled(Rect::from_center_size(center, Vec2::new(w * 2.0 + 4.0, font.size + 8.0)), 50.0, RED);
        painter.text(center, Align2::CENTER_CENTER, text, theme::bold_font(font.size), Color32::WHITE);
    } else {
        painter.text(center, Align2::CENTER_CENTER, text, font, color);
    }
}

fn draw_banner(ui: &Ui, painter: &egui::Painter, rect: Rect, ev: &Event, color: Color32, selected: bool, p: &Palette) {
    painter.rect_filled(rect, 4.0, p.event_fill(color, selected));
    let text = if ev.all_day {
        ev.title.clone()
    } else {
        format!("{} {}", fmt_time(ev.start.time()), ev.title)
    };
    draw_text(
        ui,
        painter,
        rect.left_top() + Vec2::new(6.0, 2.5),
        &text,
        theme::bold_font(11.5),
        p.event_text(color, selected),
        rect.width() - 10.0,
    );
}

// ---------------------------------------------------------------------------
// Month

pub fn month(app: &mut App, ui: &mut Ui, p: &Palette) {
    let first = first_of_month(app.date);
    let grid_start = start_of_week(first);
    let last = first + Duration::days(days_in_month(first) as i64 - 1);
    let weeks = (last - grid_start).num_days() / 7 + 1;

    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let painter = ui.painter_at(rect);
    let header_h = 26.0;
    let cell_w = rect.width() / 7.0;
    let cell_h = (rect.height() - header_h) / weeks as f32;

    for i in 0..7 {
        let x = rect.left() + (i as f32 + 1.0) * cell_w - 8.0;
        painter.text(
            Pos2::new(x, rect.top() + header_h / 2.0),
            Align2::RIGHT_CENTER,
            weekday_short(i),
            theme::font(12.0),
            p.secondary,
        );
    }
    painter.hline(rect.x_range(), rect.top() + header_h, Stroke::new(1.0_f32, p.grid));

    let events = events_in(app, grid_start, weeks * 7);
    let row_h = 18.0;

    for w in 0..weeks {
        let week_start = grid_start + Duration::days(w * 7);
        let top = rect.top() + header_h + w as f32 * cell_h;
        let lanes = banner_lanes(&events, week_start, 7);
        let max_rows = (((cell_h - 30.0) / row_h).floor() as usize).max(1);

        // Cells: background, numbers, and click targets.
        for d in 0..7 {
            let day = week_start + Duration::days(d);
            let cell = Rect::from_min_size(Pos2::new(rect.left() + d as f32 * cell_w, top), Vec2::new(cell_w, cell_h));
            if d == 0 || d == 6 {
                painter.rect_filled(cell, 0.0, p.weekend);
            }
            painter.hline(cell.x_range(), cell.bottom(), Stroke::new(1.0_f32, p.grid));
            if d > 0 {
                painter.vline(cell.left(), cell.y_range(), Stroke::new(1.0_f32, p.grid));
            }
            let in_month = day.month() == first.month();
            let label = if day.day() == 1 { day.format("%b %-d").to_string() } else { day.day().to_string() };
            let color = if in_month { p.text } else { p.tertiary };
            let font = theme::font(14.0);
            let w = text_galley(ui, &label, font.clone(), color, 200.0).size().x;
            let center = Pos2::new(cell.right() - 10.0 - w / 2.0, cell.top() + 14.0);
            day_number_badge(&painter, center, &label, day == app.today, font, color);

            let resp = ui.interact(cell, ui.id().with(("cell", day)), Sense::click());
            if resp.double_clicked() {
                let start = midnight(day) + Duration::hours(9);
                app.actions.push(Action::New { start, end: start + Duration::hours(1), all_day: false });
            } else if resp.clicked() {
                app.actions.push(Action::Select(None));
                let num = Rect::from_center_size(center, Vec2::new(w + 12.0, 22.0));
                if resp.interact_pointer_pos().is_some_and(|pos| num.contains(pos)) {
                    app.actions.push(Action::GoTo(day, View::Day));
                }
            }
        }

        // Banners spanning across the week row.
        let mut used = [0usize; 7];
        let mut hidden = [0usize; 7];
        for &(i, c0, c1, lane) in &lanes {
            for c in c0..=c1 {
                used[c as usize] = used[c as usize].max(lane + 1);
            }
            if lane + 1 >= max_rows {
                for c in c0..=c1 {
                    hidden[c as usize] += 1;
                }
                continue;
            }
            let ev = &events[i];
            let r = Rect::from_min_max(
                Pos2::new(rect.left() + c0 as f32 * cell_w + 3.0, top + 28.0 + lane as f32 * row_h),
                Pos2::new(rect.left() + (c1 + 1) as f32 * cell_w - 3.0, top + 28.0 + lane as f32 * row_h + row_h - 2.0),
            );
            let selected = app.selected.as_deref() == Some(ev.id.as_str());
            draw_banner(ui, &painter, r, ev, event_color(app, ev), selected, p);
            event_click(app, ui, r, ev, ("mb", w));
        }

        // Timed events in each day cell.
        for d in 0..7 {
            let day = week_start + Duration::days(d);
            let left = rect.left() + d as f32 * cell_w;
            let timed: Vec<&Event> = events.iter().filter(|e| !e.is_banner() && e.occurs_on(day)).collect();
            let mut row = used[d as usize];
            let mut more = hidden[d as usize];
            for (k, ev) in timed.iter().enumerate() {
                let remaining = timed.len() - k;
                if row + 1 >= max_rows && remaining > 1 || row >= max_rows {
                    more += remaining;
                    break;
                }
                let r = Rect::from_min_size(
                    Pos2::new(left + 3.0, top + 28.0 + row as f32 * row_h),
                    Vec2::new(cell_w - 6.0, row_h - 2.0),
                );
                let color = event_color(app, ev);
                let selected = app.selected.as_deref() == Some(ev.id.as_str());
                if selected {
                    painter.rect_filled(r, 4.0, color);
                }
                let text_color = if selected { Color32::WHITE } else { p.text };
                painter.circle_filled(Pos2::new(r.left() + 6.0, r.center().y), 3.5, if selected { Color32::WHITE } else { color });
                let time = fmt_time(ev.start.time());
                let tw = text_galley(ui, &time, theme::font(11.0), p.secondary, 100.0).size().x;
                let show_time = r.width() > tw + 60.0;
                if show_time {
                    painter.text(
                        Pos2::new(r.right() - 4.0, r.center().y),
                        Align2::RIGHT_CENTER,
                        &time,
                        theme::font(11.0),
                        if selected { Color32::WHITE } else { p.secondary },
                    );
                }
                let max_w = r.width() - 16.0 - if show_time { tw + 8.0 } else { 0.0 };
                draw_text(ui, &painter, Pos2::new(r.left() + 13.0, r.top() + 1.5), &ev.title, theme::font(12.0), text_color, max_w);
                event_click(app, ui, r, ev, ("mt", day));
                row += 1;
            }
            if more > 0 {
                let pos = Pos2::new(left + 10.0, top + 28.0 + row.min(max_rows - 1) as f32 * row_h + 1.0);
                let r = Rect::from_min_size(pos, Vec2::new(cell_w - 20.0, row_h - 2.0));
                painter.text(pos, Align2::LEFT_TOP, format!("{more} more"), theme::bold_font(11.5), p.secondary);
                if ui.interact(r, ui.id().with(("more", day)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    app.actions.push(Action::GoTo(day, View::Day));
                }
            }
        }
    }
}

fn event_click(app: &mut App, ui: &Ui, r: Rect, ev: &Event, salt: impl std::hash::Hash) {
    let resp = ui.interact(r, ui.id().with(("ev", &ev.id, salt)), Sense::click());
    if resp.double_clicked() {
        app.actions.push(Action::Open(ev.id.clone()));
    } else if resp.clicked() {
        app.actions.push(Action::Select(Some(ev.id.clone())));
    }
    resp.on_hover_text(hover_text(ev));
}

fn hover_text(ev: &Event) -> String {
    let when = if ev.all_day {
        let last = last_day(ev.start, ev.end);
        if last == ev.start.date() {
            format!("{} (all-day)", ev.start.format("%a %b %-d"))
        } else {
            format!("{} – {} (all-day)", ev.start.format("%a %b %-d"), last.format("%a %b %-d"))
        }
    } else if ev.start.date() == ev.end.date() {
        format!("{} {} – {}", ev.start.format("%a %b %-d"), fmt_time(ev.start.time()), fmt_time(ev.end.time()))
    } else {
        format!(
            "{} {} – {} {}",
            ev.start.format("%a %b %-d"),
            fmt_time(ev.start.time()),
            ev.end.format("%a %b %-d"),
            fmt_time(ev.end.time())
        )
    };
    let mut s = format!("{}\n{}", ev.title, when);
    if !ev.location.is_empty() {
        s.push('\n');
        s.push_str(&ev.location);
    }
    s
}

// ---------------------------------------------------------------------------
// Day / Week time grid

pub fn time_grid(app: &mut App, ui: &mut Ui, p: &Palette, ndays: i64) {
    let first = if ndays == 7 { start_of_week(app.date) } else { app.date };
    let days: Vec<NaiveDate> = (0..ndays).map(|i| first + Duration::days(i)).collect();
    let events = events_in(app, first, ndays);

    let full = ui.available_rect_before_wrap();
    let col_w = (full.width() - GUTTER) / ndays as f32;
    let col_x = |i: i64| full.left() + GUTTER + i as f32 * col_w;

    // ---- Column headers
    let header_h = 34.0;
    let (header, _) = ui.allocate_exact_size(Vec2::new(full.width(), header_h), Sense::hover());
    let painter = ui.painter();
    for (i, day) in days.iter().enumerate() {
        let x = col_x(i as i64);
        let is_today = *day == app.today;
        let wd = weekday_short(day.weekday().num_days_from_sunday() as usize);
        let num = day.day().to_string();
        let color = if is_today { RED } else { p.text };
        let font = theme::font(15.0);
        let cy = header.center().y;
        if ndays == 1 {
            let x0 = x + 8.0;
            let w = draw_text(ui, painter, Pos2::new(x0, cy - 9.0), wd, font.clone(), color, 100.0);
            let nw = text_galley(ui, &num, font.clone(), color, 50.0).size().x;
            day_number_badge(painter, Pos2::new(x0 + w + 8.0 + nw / 2.0, cy), &num, is_today, font, color);
        } else {
            let label_w = text_galley(ui, wd, font.clone(), color, 100.0).size().x;
            let nw = text_galley(ui, &num, font.clone(), color, 50.0).size().x;
            let total = label_w + 6.0 + nw;
            let x0 = x + (col_w - total) / 2.0;
            painter.text(Pos2::new(x0, cy), Align2::LEFT_CENTER, wd, font.clone(), color);
            day_number_badge(painter, Pos2::new(x0 + label_w + 6.0 + nw / 2.0, cy), &num, is_today, font, color);
            let hdr_cell = Rect::from_min_size(Pos2::new(x, header.top()), Vec2::new(col_w, header_h));
            if ui.interact(hdr_cell, ui.id().with(("hdr", i)), Sense::click()).double_clicked() {
                app.actions.push(Action::GoTo(*day, View::Day));
            }
        }
    }

    // ---- All-day lane
    let lanes = banner_lanes(&events, first, ndays);
    let n_lanes = lanes.iter().map(|l| l.3 + 1).max().unwrap_or(0).max(1);
    let lane_h = n_lanes as f32 * BANNER_H + 6.0;
    let (allday, allday_resp) = ui.allocate_exact_size(Vec2::new(full.width(), lane_h), Sense::click());
    let painter = ui.painter();
    painter.hline(full.x_range(), allday.top(), Stroke::new(1.0_f32, p.grid));
    painter.hline(full.x_range(), allday.bottom(), Stroke::new(1.0_f32, p.grid_strong));
    painter.text(
        Pos2::new(allday.left() + GUTTER - 8.0, allday.top() + 12.0),
        Align2::RIGHT_CENTER,
        "all-day",
        theme::font(11.0),
        p.secondary,
    );
    for i in 1..ndays {
        painter.vline(col_x(i), allday.y_range(), Stroke::new(1.0_f32, p.grid));
    }
    if allday_resp.double_clicked() {
        if let Some(pos) = allday_resp.interact_pointer_pos() {
            let i = (((pos.x - col_x(0)) / col_w).floor() as i64).clamp(0, ndays - 1);
            let day = first + Duration::days(i);
            app.actions.push(Action::New { start: midnight(day), end: midnight(day) + Duration::days(1), all_day: true });
        }
    } else if allday_resp.clicked() {
        app.actions.push(Action::Select(None));
    }
    for &(i, c0, c1, lane) in &lanes {
        let ev = &events[i];
        let r = Rect::from_min_max(
            Pos2::new(col_x(c0) + 2.0, allday.top() + 3.0 + lane as f32 * BANNER_H),
            Pos2::new(col_x(c1 + 1) - 2.0, allday.top() + 3.0 + lane as f32 * BANNER_H + BANNER_H - 2.0),
        );
        let selected = app.selected.as_deref() == Some(ev.id.as_str());
        draw_banner(ui, ui.painter(), r, ev, event_color(app, ev), selected, p);
        event_click(app, ui, r, ev, "allday");
    }

    // ---- Scrolling hour grid
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if let Some(off) = app.scroll_to.take() {
        scroll = scroll.vertical_scroll_offset(off);
    }
    scroll.show(ui, |ui| {
        let (grid, bg) = ui.allocate_exact_size(Vec2::new(full.width(), 24.0 * HOUR_H + 12.0), Sense::click_and_drag());
        let top = grid.top() + 6.0;
        let painter = ui.painter_at(grid);

        for (i, day) in days.iter().enumerate() {
            let wd = day.weekday().num_days_from_sunday();
            if ndays > 1 && (wd == 0 || wd == 6) {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(col_x(i as i64), grid.top()), Vec2::new(col_w, grid.height())),
                    0.0,
                    p.weekend,
                );
            }
        }
        let now = Local::now().naive_local();
        let now_y = top + minute_of_day(now) as f32 / 60.0 * HOUR_H;
        let today_col = days.iter().position(|d| *d == app.today);
        for h in 0..=24 {
            let y = top + h as f32 * HOUR_H;
            painter.hline((grid.left() + GUTTER - 4.0)..=grid.right(), y, Stroke::new(1.0_f32, p.grid));
            let near_now = today_col.is_some() && (y - now_y).abs() < 10.0;
            if h > 0 && h < 24 && !near_now {
                painter.text(
                    Pos2::new(grid.left() + GUTTER - 8.0, y),
                    Align2::RIGHT_CENTER,
                    fmt_hour(h),
                    theme::font(11.0),
                    p.secondary,
                );
            }
        }
        for i in 0..ndays {
            painter.vline(col_x(i), grid.y_range(), Stroke::new(1.0_f32, p.grid));
        }

        let pointer_time = |pos: Pos2| -> NaiveDateTime {
            let i = (((pos.x - col_x(0)) / col_w).floor() as i64).clamp(0, ndays - 1);
            let mins = (((pos.y - top) / HOUR_H) * 60.0).clamp(0.0, 24.0 * 60.0 - 1.0) as i64;
            midnight(first + Duration::days(i)) + Duration::minutes(mins)
        };

        // Background interactions: click to deselect, double-click or drag to create.
        if bg.double_clicked() {
            if let Some(pos) = bg.interact_pointer_pos() {
                let t = pointer_time(pos);
                let start = midnight(t.date()) + Duration::minutes(minute_of_day(t) / 30 * 30);
                app.actions.push(Action::New { start, end: start + Duration::hours(1), all_day: false });
            }
        } else if bg.clicked() {
            app.actions.push(Action::Select(None));
        }
        if bg.drag_started() {
            if let Some(origin) = ui.input(|i| i.pointer.press_origin()) {
                let t = snap(pointer_time(origin));
                app.drag = Some(Drag::Create { anchor: t, current: t });
            }
        }
        if let (Some(Drag::Create { anchor, current }), Some(pos)) = (&mut app.drag, bg.interact_pointer_pos()) {
            let t = snap(pointer_time(pos));
            *current = if t.date() == anchor.date() { t } else { midnight(anchor.date()) + Duration::minutes(minute_of_day(t)) };
        }
        if bg.drag_stopped() {
            if let Some(Drag::Create { anchor, current }) = app.drag.take() {
                let (mut s, mut e) = if current < anchor { (current, anchor) } else { (anchor, current) };
                if e - s < Duration::minutes(15) {
                    s = anchor;
                    e = anchor + Duration::hours(1);
                }
                app.actions.push(Action::New { start: s, end: e, all_day: false });
            }
        }

        // Timed events, laid out side by side when they overlap.
        let timed: Vec<&Event> = events.iter().filter(|e| !e.is_banner()).collect();
        for (i, day) in days.iter().enumerate() {
            let segs = layout_day(&timed, *day);
            for seg in segs {
                let ev = seg.ev;
                let x0 = col_x(i as i64) + 1.0 + seg.col as f32 * (col_w - 6.0) / seg.cols as f32;
                let w = (col_w - 6.0) / seg.cols as f32 - 1.0;
                let y0 = top + seg.start_min as f32 / 60.0 * HOUR_H + 1.0;
                let y1 = (top + seg.end_min as f32 / 60.0 * HOUR_H - 1.0).max(y0 + 16.0);
                let r = Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x0 + w, y1));
                let color = event_color(app, ev);
                let selected = app.selected.as_deref() == Some(ev.id.as_str());
                draw_timed(ui, &painter, r, ev, color, selected, p);

                let resp = ui.interact(r, ui.id().with(("tev", &ev.id, i)), Sense::click_and_drag());
                let editable = !is_read_only(app, ev);
                let near_bottom = |pos: Pos2| pos.y > r.bottom() - 6.0 && seg.ends_here;
                if editable && resp.hovered() {
                    if let Some(pos) = resp.hover_pos() {
                        if near_bottom(pos) {
                            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
                        }
                    }
                }
                if resp.double_clicked() {
                    app.actions.push(Action::Open(ev.id.clone()));
                } else if resp.clicked() {
                    app.actions.push(Action::Select(Some(ev.id.clone())));
                }
                if editable && resp.drag_started() {
                    let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(r.center());
                    app.selected = Some(ev.id.clone());
                    app.drag = Some(if near_bottom(origin) {
                        Drag::Resize { id: ev.id.clone() }
                    } else {
                        Drag::Move { id: ev.id.clone(), grab: pointer_time(origin) - ev.start }
                    });
                }
                if resp.drag_stopped() {
                    if let Some(Drag::Move { id, .. } | Drag::Resize { id }) = app.drag.take() {
                        app.actions.push(Action::Committed(id));
                    }
                }
                resp.on_hover_text(hover_text(ev));
            }
        }

        // Apply an in-progress move/resize directly to the event.
        if let Some(pos) = ui.ctx().pointer_latest_pos() {
            match &app.drag {
                Some(Drag::Move { id, grab }) => {
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    let new_start = snap(pointer_time(pos) - *grab);
                    if let Some(ev) = app.store.event_mut(id) {
                        let len = ev.end - ev.start;
                        ev.start = new_start;
                        ev.end = new_start + len;
                    }
                }
                Some(Drag::Resize { id }) => {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
                    if let Some(ev) = app.store.event_mut(id) {
                        ev.end = snap(pointer_time(pos)).max(ev.start + Duration::minutes(15));
                    }
                }
                _ => {}
            }
        }
        // A drag that ended outside any widget (e.g. released over another one).
        if app.drag.is_some() && !ui.input(|i| i.pointer.any_down()) {
            match app.drag.take() {
                Some(Drag::Move { id, .. } | Drag::Resize { id }) => app.actions.push(Action::Committed(id)),
                Some(other) => app.drag = Some(other),
                None => {}
            }
        }

        // Ghost block while drag-creating.
        if let Some(Drag::Create { anchor, current }) = &app.drag {
            let (s, e) = if current < anchor { (*current, *anchor) } else { (*anchor, *current) };
            if let Some(i) = days.iter().position(|d| *d == s.date()) {
                let y0 = top + minute_of_day(s) as f32 / 60.0 * HOUR_H;
                let y1 = (top + minute_of_day(e) as f32 / 60.0 * HOUR_H).max(y0 + HOUR_H / 4.0);
                let r = Rect::from_min_max(Pos2::new(col_x(i as i64) + 1.0, y0), Pos2::new(col_x(i as i64) + col_w - 5.0, y1));
                let ghost = Event {
                    id: String::new(),
                    calendar_id: String::new(),
                    title: "New Event".into(),
                    start: s,
                    end: e.max(s + Duration::minutes(15)),
                    all_day: false,
                    location: String::new(),
                    notes: String::new(),
                    remote_id: None,
                    recurring_id: None,
                    pending: false,
                    rev: 0,
                };
                draw_timed(ui, &painter, r, &ghost, theme::ACCENT, false, p);
            }
        }

        // Current time indicator.
        if let Some(i) = today_col {
            let x0 = col_x(i as i64);
            painter.hline((grid.left() + GUTTER)..=grid.right(), now_y, Stroke::new(1.0_f32, RED.gamma_multiply(0.35)));
            painter.hline(x0..=x0 + col_w, now_y, Stroke::new(2.0_f32, RED));
            painter.circle_filled(Pos2::new(x0, now_y), 4.0, RED);
            painter.text(
                Pos2::new(grid.left() + GUTTER - 8.0, now_y),
                Align2::RIGHT_CENTER,
                fmt_time(now.time()).replace(" AM", "").replace(" PM", ""),
                theme::bold_font(11.0),
                RED,
            );
        }
    });
}

struct Segment<'a> {
    ev: &'a Event,
    start_min: i64,
    end_min: i64,
    col: usize,
    cols: usize,
    ends_here: bool,
}

/// Clips events to `day` and assigns side-by-side columns to overlapping ones.
fn layout_day<'a>(events: &[&'a Event], day: NaiveDate) -> Vec<Segment<'a>> {
    let ds = midnight(day);
    let de = ds + Duration::days(1);
    let mut segs: Vec<Segment> = events
        .iter()
        .filter(|e| e.occurs_on(day))
        .map(|e| {
            let s = e.start.max(ds);
            let end = e.end.min(de);
            let start_min = (s - ds).num_minutes();
            let end_min = ((end - ds).num_minutes()).max(start_min + 15);
            Segment { ev: e, start_min, end_min, col: 0, cols: 1, ends_here: e.end <= de }
        })
        .collect();
    segs.sort_by_key(|s| (s.start_min, -s.end_min));

    let mut cluster_start = 0;
    let mut cluster_end = i64::MIN;
    let mut col_ends: Vec<i64> = Vec::new();
    for i in 0..=segs.len() {
        let new_cluster = i == segs.len() || segs[i].start_min >= cluster_end;
        if new_cluster && i > cluster_start {
            let n = col_ends.len().max(1);
            for s in &mut segs[cluster_start..i] {
                s.cols = n;
            }
        }
        if i == segs.len() {
            break;
        }
        if new_cluster {
            cluster_start = i;
            col_ends.clear();
            cluster_end = i64::MIN;
        }
        let col = match col_ends.iter().position(|&end| end <= segs[i].start_min) {
            Some(c) => c,
            None => {
                col_ends.push(0);
                col_ends.len() - 1
            }
        };
        col_ends[col] = segs[i].end_min;
        segs[i].col = col;
        cluster_end = cluster_end.max(segs[i].end_min);
    }
    segs
}

fn draw_timed(ui: &Ui, painter: &egui::Painter, r: Rect, ev: &Event, color: Color32, selected: bool, p: &Palette) {
    painter.rect_filled(r, 4.0, p.event_fill(color, selected));
    let bar = Rect::from_min_size(r.min, Vec2::new(3.0, r.height()));
    painter.rect_filled(bar, egui::Rounding { nw: 4.0, sw: 4.0, ne: 0.0, se: 0.0 }, color);
    let text_color = p.event_text(color, selected);
    let max_w = r.width() - 10.0;
    let x = r.left() + 7.0;
    let mut y = r.top() + 3.0;
    let p_clip = painter.with_clip_rect(r.intersect(painter.clip_rect()));
    if r.height() < 30.0 {
        let line = format!("{}  {}", ev.title, fmt_time(ev.start.time()));
        draw_text(ui, &p_clip, Pos2::new(x, y - 1.0), &line, theme::bold_font(11.5), text_color, max_w);
        return;
    }
    draw_text(ui, &p_clip, Pos2::new(x, y), &ev.title, theme::bold_font(12.0), text_color, max_w);
    y += 16.0;
    if !ev.location.is_empty() && r.height() > 48.0 {
        draw_text(ui, &p_clip, Pos2::new(x, y), &ev.location, theme::font(11.5), text_color, max_w);
        y += 15.0;
    }
    if y + 14.0 < r.bottom() {
        draw_text(ui, &p_clip, Pos2::new(x, y), &fmt_time(ev.start.time()), theme::font(11.5), text_color, max_w);
    }
}

// ---------------------------------------------------------------------------
// Year

pub fn year(app: &mut App, ui: &mut Ui, p: &Palette) {
    let rect = ui.available_rect_before_wrap().shrink2(Vec2::new(16.0, 4.0));
    ui.allocate_rect(rect, Sense::hover());
    let (cols, rows) = if rect.width() > rect.height() * 1.1 { (4, 3) } else { (3, 4) };
    let cw = rect.width() / cols as f32;
    let ch = rect.height() / rows as f32;
    let jan = NaiveDate::from_ymd_opt(app.date.year(), 1, 1).unwrap();
    for m in 0..12 {
        let cell = Rect::from_min_size(
            Pos2::new(rect.left() + (m % cols) as f32 * cw, rect.top() + (m / cols) as f32 * ch),
            Vec2::new(cw, ch),
        )
        .shrink2(Vec2::new(14.0, 8.0));
        let month = add_months(jan, m as i32);
        let is_cur = month.year() == app.today.year() && month.month() == app.today.month();
        let title_rect = Rect::from_min_size(cell.min, Vec2::new(cell.width(), 26.0));
        ui.painter().text(
            title_rect.left_center(),
            Align2::LEFT_CENTER,
            month.format("%B").to_string(),
            theme::bold_font(17.0),
            if is_cur { RED } else { p.text },
        );
        if ui
            .interact(title_rect, ui.id().with(("ym", m)), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            app.actions.push(Action::GoTo(month, View::Month));
        }
        let grid = Rect::from_min_max(Pos2::new(cell.left(), title_rect.bottom() + 4.0), cell.max);
        if let Some(day) = mini_month(ui, grid, month, app.today, None, p, false) {
            app.actions.push(Action::GoTo(day, View::Day));
        }
    }
}

/// Draws a compact month grid in `rect`; returns a clicked day.
pub fn mini_month(
    ui: &mut Ui,
    rect: Rect,
    month: NaiveDate,
    today: NaiveDate,
    selected: Option<NaiveDate>,
    p: &Palette,
    show_outside: bool,
) -> Option<NaiveDate> {
    let first = first_of_month(month);
    let start = start_of_week(first);
    let cw = rect.width() / 7.0;
    let ch = (rect.height() / 7.0).min(cw * 1.1);
    let size = (ch * 0.48).clamp(9.0, 13.0);
    let painter = ui.painter_at(rect.expand(4.0));
    for i in 0..7 {
        painter.text(
            Pos2::new(rect.left() + (i as f32 + 0.5) * cw, rect.top() + ch / 2.0),
            Align2::CENTER_CENTER,
            weekday_initial(i),
            theme::bold_font(size * 0.85),
            p.secondary,
        );
    }
    let mut clicked = None;
    for k in 0..42 {
        let day = start + Duration::days(k);
        let in_month = day.month() == first.month();
        if !in_month && !show_outside {
            continue;
        }
        let center = Pos2::new(
            rect.left() + ((k % 7) as f32 + 0.5) * cw,
            rect.top() + ((k / 7) as f32 + 1.5) * ch,
        );
        let r = Rect::from_center_size(center, Vec2::new(cw, ch));
        let resp = ui.interact(r, ui.id().with(("mini", first, k)), Sense::click());
        let is_today = day == today;
        let is_sel = selected == Some(day);
        let radius = (ch * 0.46).min(cw * 0.46);
        if is_today {
            painter.circle_filled(center, radius, RED);
        } else if is_sel {
            painter.circle_filled(center, radius, p.grid_strong);
        } else if resp.hovered() {
            painter.circle_filled(center, radius, p.selection);
        }
        let color = if is_today {
            Color32::WHITE
        } else if in_month {
            p.text
        } else {
            p.tertiary
        };
        let font = if is_today || is_sel { theme::bold_font(size) } else { theme::font(size) };
        painter.text(center, Align2::CENTER_CENTER, day.day().to_string(), font, color);
        if resp.clicked() {
            clicked = Some(day);
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, s: (u32, u32), e: (u32, u32)) -> Event {
        let day = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        Event {
            id: id.into(),
            calendar_id: "c".into(),
            title: id.into(),
            start: day.and_hms_opt(s.0, s.1, 0).unwrap(),
            end: day.and_hms_opt(e.0, e.1, 0).unwrap(),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
        }
    }

    #[test]
    fn overlapping_events_share_columns() {
        let a = ev("a", (13, 0), (14, 30));
        let b = ev("b", (13, 30), (14, 15));
        let c = ev("c", (16, 0), (17, 0));
        let all = [&a, &b, &c];
        let segs = layout_day(&all, NaiveDate::from_ymd_opt(2026, 11, 10).unwrap());
        let get = |id: &str| segs.iter().find(|s| s.ev.id == id).unwrap();
        assert_eq!((get("a").col, get("a").cols), (0, 2));
        assert_eq!((get("b").col, get("b").cols), (1, 2));
        assert_eq!((get("c").col, get("c").cols), (0, 1));
    }
}
