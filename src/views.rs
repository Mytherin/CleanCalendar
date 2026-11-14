//! The calendar views: month grid, day/week time grid, year overview, and
//! the sidebar mini-month.
//!
//! The month and day/week views only paint here. Where things go comes from
//! `layout.rs`, and what the pointer does comes from `interact.rs`;
//! [`drive_pointer`] is the thin adapter that feeds egui's input to it.

use std::sync::Arc;

use chrono::{Datelike, Duration, Local, NaiveDate};
use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, CursorIcon, FontId, Galley, PointerButton, Pos2, Rect, Sense, Stroke, Ui,
    Vec2,
};

use crate::app::{Action, App, View};
use crate::dates::*;
use crate::interact::{Cursor, Input};
use crate::layout::{self, HitMap, Target, WeekLayout, WeekRects, DAY_HEADER_H, GUTTER, HOUR_H};
use crate::model::Event;
use crate::theme::{self, Palette, RED};

/// Seconds the pointer must rest on an event before its tooltip shows.
const TOOLTIP_DELAY: f32 = 0.5;

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

fn is_selected(app: &App, ev: &Event) -> bool {
    app.selected.as_deref() == Some(ev.id.as_str())
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
// Pointer adapter

/// Feeds this frame's pointer events to the interaction state machine, queues
/// the resulting actions, and shows hover feedback. `hits` must describe what
/// is drawn this frame.
fn drive_pointer(app: &mut App, ui: &Ui, hits: &HitMap) {
    let ctx = ui.ctx().clone();
    let layer = ui.layer_id();
    // Presses over windows or popups on top of this view aren't ours.
    let ours = |pos: Pos2| ctx.layer_id_at(pos).map_or(true, |l| l == layer);
    let (events, time, any_down, latest) =
        ctx.input(|i| (i.events.clone(), i.time, i.pointer.any_down(), i.pointer.latest_pos()));

    let mut actions = Vec::new();
    let mut released = false;
    for event in events {
        let input = match event {
            egui::Event::PointerMoved(pos) => Input::Move { pos },
            egui::Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, .. } => {
                if !ours(pos) {
                    continue;
                }
                Input::Press { pos }
            }
            egui::Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, .. } => {
                released = true;
                Input::Release { pos, time }
            }
            _ => continue,
        };
        actions.extend(app.pointer.handle(input, hits));
    }
    // The release happened somewhere we didn't hear about (e.g. outside the window).
    if app.pointer.is_active() && !any_down && !released {
        if let Some(pos) = latest {
            actions.extend(app.pointer.handle(Input::Release { pos, time }, hits));
        }
    }

    let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|&pos| ours(pos));
    let target = hover.and_then(|pos| hits.hit(pos));
    if let Some(cursor) = app.pointer.cursor(target) {
        ctx.set_cursor_icon(match cursor {
            Cursor::Grabbing => CursorIcon::Grabbing,
            Cursor::ResizeVertical => CursorIcon::ResizeVertical,
            Cursor::PointingHand => CursorIcon::PointingHand,
        });
    }
    if let (false, Some(Target::Event { id, .. })) = (app.pointer.is_active(), target) {
        let still = ctx.input(|i| i.pointer.time_since_last_movement());
        if still < TOOLTIP_DELAY {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(TOOLTIP_DELAY - still));
        } else if let Some(ev) = app.store.event(id) {
            let text = hover_text(ev);
            egui::show_tooltip_at_pointer(&ctx, layer, egui::Id::new(("event-tip", id)), |ui| ui.label(text));
        }
    }

    if !actions.is_empty() {
        // Actions apply after this frame is drawn; show their effect promptly.
        ctx.request_repaint();
    }
    app.actions.extend(actions);
}

// ---------------------------------------------------------------------------
// Month

fn month_label(day: NaiveDate) -> String {
    if day.day() == 1 {
        day.format("%b %-d").to_string()
    } else {
        day.day().to_string()
    }
}

pub fn month(app: &mut App, ui: &mut Ui, p: &Palette) {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let (grid_start, weeks) = layout::month_span(app.date);
    let events = layout::events_in(&app.store, grid_start, weeks * 7);
    let number_font = theme::font(14.0);
    let number_w = |day: NaiveDate| text_galley(ui, &month_label(day), number_font.clone(), p.text, 200.0).size().x;
    let lay = layout::month(&events, app.date, rect, &number_w);

    drive_pointer(app, ui, &lay.hits);

    let painter = ui.painter_at(rect);
    for i in 0..7 {
        let x = rect.left() + (i as f32 + 1.0) * lay.cell_w - 8.0;
        painter.text(
            Pos2::new(x, rect.top() + lay.header_h / 2.0),
            Align2::RIGHT_CENTER,
            weekday_short(i),
            theme::font(12.0),
            p.secondary,
        );
    }
    painter.hline(rect.x_range(), rect.top() + lay.header_h, Stroke::new(1.0_f32, p.grid));

    for cell in &lay.cells {
        let wd = cell.day.weekday().num_days_from_sunday();
        if wd == 0 || wd == 6 {
            painter.rect_filled(cell.rect, 0.0, p.weekend);
        }
        painter.hline(cell.rect.x_range(), cell.rect.bottom(), Stroke::new(1.0_f32, p.grid));
        if wd > 0 {
            painter.vline(cell.rect.left(), cell.rect.y_range(), Stroke::new(1.0_f32, p.grid));
        }
        let color = if cell.in_month { p.text } else { p.tertiary };
        day_number_badge(&painter, cell.number.center(), &month_label(cell.day), cell.day == app.today, number_font.clone(), color);
    }

    for b in &lay.banners {
        let ev = &events[b.ev];
        draw_banner(ui, &painter, b.rect, ev, event_color(app, ev), is_selected(app, ev), p);
    }

    for row in &lay.rows {
        let (ev, r) = (&events[row.ev], row.rect);
        let color = event_color(app, ev);
        let selected = is_selected(app, ev);
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
    }

    for &(_, more, r) in &lay.more {
        painter.text(r.min, Align2::LEFT_TOP, format!("{more} more"), theme::bold_font(11.5), p.secondary);
    }
}

// ---------------------------------------------------------------------------
// Day / Week time grid

pub fn time_grid(app: &mut App, ui: &mut Ui, p: &Palette, ndays: i64) {
    let first = if ndays == 7 { start_of_week(app.date) } else { app.date };
    let events = layout::events_in(&app.store, first, ndays);

    let full = ui.available_rect_before_wrap();
    let (header, _) = ui.allocate_exact_size(Vec2::new(full.width(), DAY_HEADER_H), Sense::hover());
    let lane_h = layout::allday_height(&events, first, ndays);
    let (allday, _) = ui.allocate_exact_size(Vec2::new(full.width(), lane_h), Sense::hover());
    // Header and all-day lane stay put; paint them with the outer painter.
    let outer = ui.painter().clone();

    // Our own pointer handling does drags, so the scroll area must not.
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]).drag_to_scroll(false);
    if let Some(off) = app.scroll_to.take() {
        scroll = scroll.vertical_scroll_offset(off);
    }
    scroll.show(ui, |ui| {
        let (grid, _) = ui.allocate_exact_size(Vec2::new(full.width(), layout::grid_height()), Sense::hover());
        let rects = WeekRects { header, allday, grid, grid_clip: ui.clip_rect().intersect(grid) };
        let lay = layout::week(&events, first, ndays, rects, &|ev| !is_read_only(app, ev));

        drive_pointer(app, ui, &lay.hits);

        paint_day_headers(app, ui, &outer, &lay, p);
        paint_allday(app, ui, &outer, &lay, &events, p);
        paint_hour_grid(app, ui, &lay, &events, p);
    });
}

fn paint_day_headers(app: &App, ui: &Ui, painter: &egui::Painter, lay: &WeekLayout, p: &Palette) {
    let g = &lay.geom;
    let cy = lay.rects.header.center().y;
    for i in 0..lay.ndays {
        let day = lay.first + Duration::days(i);
        let x = g.col_x(i);
        let is_today = day == app.today;
        let wd = weekday_short(day.weekday().num_days_from_sunday() as usize);
        let num = day.day().to_string();
        let color = if is_today { RED } else { p.text };
        let font = theme::font(15.0);
        if lay.ndays == 1 {
            let x0 = x + 8.0;
            let w = draw_text(ui, painter, Pos2::new(x0, cy - 9.0), wd, font.clone(), color, 100.0);
            let nw = text_galley(ui, &num, font.clone(), color, 50.0).size().x;
            day_number_badge(painter, Pos2::new(x0 + w + 8.0 + nw / 2.0, cy), &num, is_today, font, color);
        } else {
            let label_w = text_galley(ui, wd, font.clone(), color, 100.0).size().x;
            let nw = text_galley(ui, &num, font.clone(), color, 50.0).size().x;
            let x0 = x + (g.col_w - (label_w + 6.0 + nw)) / 2.0;
            painter.text(Pos2::new(x0, cy), Align2::LEFT_CENTER, wd, font.clone(), color);
            day_number_badge(painter, Pos2::new(x0 + label_w + 6.0 + nw / 2.0, cy), &num, is_today, font, color);
        }
    }
}

fn paint_allday(app: &App, ui: &Ui, painter: &egui::Painter, lay: &WeekLayout, events: &[Event], p: &Palette) {
    let r = lay.rects.allday;
    let x_range = lay.rects.header.x_range();
    painter.hline(x_range, r.top(), Stroke::new(1.0_f32, p.grid));
    painter.hline(x_range, r.bottom(), Stroke::new(1.0_f32, p.grid_strong));
    painter.text(
        Pos2::new(r.left() + GUTTER - 8.0, r.top() + 12.0),
        Align2::RIGHT_CENTER,
        "all-day",
        theme::font(11.0),
        p.secondary,
    );
    for i in 1..lay.ndays {
        painter.vline(lay.geom.col_x(i), r.y_range(), Stroke::new(1.0_f32, p.grid));
    }
    for b in &lay.banners {
        let ev = &events[b.ev];
        draw_banner(ui, painter, b.rect, ev, event_color(app, ev), is_selected(app, ev), p);
    }
}

fn paint_hour_grid(app: &App, ui: &Ui, lay: &WeekLayout, events: &[Event], p: &Palette) {
    let grid = lay.rects.grid;
    let g = &lay.geom;
    let painter = ui.painter_at(grid);

    for i in 0..lay.ndays {
        let wd = (lay.first + Duration::days(i)).weekday().num_days_from_sunday();
        if lay.ndays > 1 && (wd == 0 || wd == 6) {
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(g.col_x(i), grid.top()), Vec2::new(g.col_w, grid.height())),
                0.0,
                p.weekend,
            );
        }
    }
    let now = Local::now().naive_local();
    let now_y = g.y_of(minute_of_day(now));
    let today_col = (0..lay.ndays).find(|&i| lay.first + Duration::days(i) == app.today);
    for h in 0..=24u32 {
        let y = g.y_of(h as i64 * 60);
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
    for i in 0..lay.ndays {
        painter.vline(g.col_x(i), grid.y_range(), Stroke::new(1.0_f32, p.grid));
    }

    for t in &lay.timed {
        let ev = &events[t.ev];
        draw_timed(ui, &painter, t.rect, ev, event_color(app, ev), is_selected(app, ev), p);
    }

    // Ghost block while drag-creating.
    if let Some((s, e)) = app.pointer.ghost() {
        let i = (s.date() - lay.first).num_days();
        if (0..lay.ndays).contains(&i) {
            let y0 = g.y_of(minute_of_day(s));
            let y1 = g.y_of(minute_of_day(s) + (e - s).num_minutes()).max(y0 + HOUR_H / 4.0);
            let r = Rect::from_min_max(Pos2::new(g.col_x(i) + 1.0, y0), Pos2::new(g.col_x(i) + g.col_w - 5.0, y1));
            let ghost = Event {
                id: String::new(),
                calendar_id: String::new(),
                title: "New Event".into(),
                start: s,
                end: e,
                all_day: false,
                location: String::new(),
                notes: String::new(),
                remote_id: None,
                recurring_id: None,
                pending: false,
                rev: 0,
                etag: None,
                base: None,
            };
            draw_timed(ui, &painter, r, &ghost, theme::ACCENT, false, p);
        }
    }

    // Current time indicator.
    if let Some(i) = today_col {
        let x0 = g.col_x(i);
        painter.hline((grid.left() + GUTTER)..=grid.right(), now_y, Stroke::new(1.0_f32, RED.gamma_multiply(0.35)));
        painter.hline(x0..=x0 + g.col_w, now_y, Stroke::new(2.0_f32, RED));
        painter.circle_filled(Pos2::new(x0, now_y), 4.0, RED);
        painter.text(
            Pos2::new(grid.left() + GUTTER - 8.0, now_y),
            Align2::RIGHT_CENTER,
            fmt_time(now.time()).replace(" AM", "").replace(" PM", ""),
            theme::bold_font(11.0),
            RED,
        );
    }
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

