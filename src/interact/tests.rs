//! Interaction tests: a tiny simulator lays out a view exactly like the app
//! does, feeds synthetic pointer input to [`Pointer`], and applies the live
//! `SetTimes` updates to its events between "frames", so tests see the same
//! moving targets a user would.

use chrono::NaiveDateTime;
use eframe::egui::{Pos2, Rect, Vec2};

use super::*;
use crate::layout::{self, WeekLayout, WeekRects};
use crate::model::Event;

fn t(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
}

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn event(id: &str, start: &str, end: &str) -> Event {
    Event {
        id: id.into(),
        calendar_id: "cal".into(),
        title: id.into(),
        start: t(start),
        end: t(end),
        all_day: false,
        location: String::new(),
        notes: String::new(),
        remote_id: None,
        recurring_id: None,
        pending: false,
        rev: 0,
        etag: None,
        base: None,
        recurrence: None,
        time_zone: None,
    }
}

fn all_day(id: &str, first: &str, last_exclusive: &str) -> Event {
    Event { all_day: true, ..event(id, &format!("{first} 00:00"), &format!("{last_exclusive} 00:00")) }
}

const WIDTH: f32 = 758.0; // 100px day columns after the gutter
const GRID_TOP: f32 = 100.0;

/// The week of Sun 2026-11-08 .. Sat 2026-11-14.
struct Sim {
    events: Vec<Event>,
    read_only: Vec<String>,
    first: NaiveDate,
    ndays: i64,
    /// Visible part of the hour grid; defaults to all of it.
    grid_clip: Option<Rect>,
    pointer: Pointer,
    time: f64,
    pos: Pos2,
    log: Vec<Action>,
}

impl Sim {
    fn week(events: Vec<Event>) -> Sim {
        Sim {
            events,
            read_only: Vec::new(),
            first: d("2026-11-08"),
            ndays: 7,
            grid_clip: None,
            pointer: Pointer::default(),
            time: 100.0,
            pos: Pos2::ZERO,
            log: Vec::new(),
        }
    }

    fn layout(&self) -> WeekLayout {
        let header = Rect::from_min_size(Pos2::ZERO, Vec2::new(WIDTH, layout::DAY_HEADER_H));
        let lane_h = layout::allday_height(&self.events, self.first, self.ndays);
        let allday = Rect::from_min_size(Pos2::new(0.0, header.bottom()), Vec2::new(WIDTH, lane_h));
        assert!(allday.bottom() <= GRID_TOP, "all-day lane overlaps the grid in this test setup");
        let grid = Rect::from_min_size(Pos2::new(0.0, GRID_TOP), Vec2::new(WIDTH, layout::grid_height()));
        let rects = WeekRects { header, allday, grid, grid_clip: self.grid_clip.unwrap_or(grid) };
        layout::week(&self.events, self.first, self.ndays, rects, &|e| !self.read_only.contains(&e.id))
    }

    /// One frame: lay out the current state, handle one input, apply actions.
    fn send(&mut self, input: Input) -> Vec<Action> {
        let lay = self.layout();
        let out = self.pointer.handle(input, &lay.hits);
        for a in &out {
            if let Action::SetTimes { id, start, end } = a {
                let ev = self.events.iter_mut().find(|e| &e.id == id).unwrap();
                ev.start = *start;
                ev.end = *end;
            }
        }
        self.log.extend(out.iter().cloned());
        out
    }

    fn press(&mut self, pos: Pos2) {
        self.time += 0.05;
        self.pos = pos;
        self.send(Input::Press { pos });
    }

    fn move_to(&mut self, pos: Pos2) {
        self.time += 0.016;
        self.pos = pos;
        self.send(Input::Move { pos });
    }

    fn release(&mut self) -> Vec<Action> {
        self.time += 0.05;
        self.send(Input::Release { pos: self.pos, time: self.time })
    }

    /// Press at `from`, move to `to` over several frames, release there.
    fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<Action> {
        self.press(from);
        for k in 1..=5 {
            self.move_to(from + (to - from) * (k as f32 / 5.0));
        }
        self.release()
    }

    fn click(&mut self, pos: Pos2) -> Vec<Action> {
        self.time += 1.0; // well apart from any earlier click
        self.press(pos);
        self.release()
    }

    fn double_click(&mut self, pos: Pos2) -> Vec<Action> {
        self.click(pos);
        self.press(pos);
        self.release()
    }

    /// A point in the hour grid at `day` (YYYY-MM-DD) and `hh:mm`, centered in the column.
    fn at(&self, day: &str, hhmm: &str) -> Pos2 {
        let g = self.layout().geom;
        let i = (d(day) - self.first).num_days();
        let when = t(&format!("{day} {hhmm}"));
        Pos2::new(g.col_x(i) + g.col_w / 2.0, g.y_of(minute_of_day(when)) + 1.0)
    }

    /// The center of a day column at height `y`.
    fn column(&self, day: &str, y: f32) -> Pos2 {
        let g = self.layout().geom;
        Pos2::new(g.col_x((d(day) - self.first).num_days()) + g.col_w / 2.0, y)
    }

    fn hit_rect(&self, id: &str, want: impl Fn(Grip) -> bool) -> Rect {
        let lay = self.layout();
        lay.hits
            .hits
            .iter()
            .find(|h| matches!(&h.target, Target::Event { id: x, grip, .. } if x == id && want(*grip)))
            .unwrap_or_else(|| panic!("no hit for {id}"))
            .rect
    }

    /// Center of the (first) drawn piece of an event.
    fn body(&self, id: &str) -> Pos2 {
        self.hit_rect(id, |g| g != Grip::Resize).center()
    }

    fn resize_grip(&self, id: &str) -> Pos2 {
        self.hit_rect(id, |g| g == Grip::Resize).center()
    }

    fn ev(&self, id: &str) -> (NaiveDateTime, NaiveDateTime) {
        let e = self.events.iter().find(|e| e.id == id).unwrap();
        (e.start, e.end)
    }

    fn edits(&self) -> usize {
        self.log.iter().filter(|a| matches!(a, Action::SetTimes { .. } | Action::BeginEdit(_) | Action::Committed(_))).count()
    }
}

// ---------------------------------------------------------------------------
// Week view: dragging

#[test]
fn all_day_multi_day_event_drags_by_whole_days() {
    // Mon..Wed; the banner's center is in the Tue column.
    let mut sim = Sim::week(vec![all_day("trip", "2026-11-09", "2026-11-12")]);
    let grab = sim.body("trip");
    let out = sim.drag(grab, Pos2::new(sim.column("2026-11-12", 0.0).x, grab.y));
    assert_eq!(sim.ev("trip"), (t("2026-11-11 00:00"), t("2026-11-14 00:00")));
    assert_eq!(out, vec![Action::Committed("trip".into())]);
    // The drag announced itself before changing anything (sync snapshots the event).
    assert_eq!(sim.log[..2], [Action::Select(Some("trip".into())), Action::BeginEdit("trip".into())]);
}

#[test]
fn long_timed_event_drags_by_days_keeping_its_times() {
    let mut sim = Sim::week(vec![event("conf", "2026-11-09 15:00", "2026-11-11 10:00")]);
    let grab = sim.body("conf");
    sim.drag(grab, grab + Vec2::new(-100.0, 0.0));
    assert_eq!(sim.ev("conf"), (t("2026-11-08 15:00"), t("2026-11-10 10:00")));
}

#[test]
fn banner_drag_clamps_to_the_visible_week() {
    let mut sim = Sim::week(vec![all_day("trip", "2026-11-12", "2026-11-14")]);
    let grab = sim.column("2026-11-12", sim.body("trip").y);
    sim.drag(grab, grab + Vec2::new(900.0, 0.0));
    // Grabbed in the Thu column, dropped past Sat: Thu -> Sat is +2 days.
    assert_eq!(sim.ev("trip"), (t("2026-11-14 00:00"), t("2026-11-16 00:00")));
}

#[test]
fn timed_event_moves_across_days_and_hours() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 09:30")]);
    let grab = sim.body("standup");
    let out = sim.drag(grab, grab + Vec2::new(200.0, 4.0 * layout::HOUR_H));
    assert_eq!(sim.ev("standup"), (t("2026-11-12 13:00"), t("2026-11-12 13:30")));
    assert_eq!(out, vec![Action::Committed("standup".into())]);
}

#[test]
fn moves_snap_to_quarter_hours() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 09:30")]);
    let grab = sim.body("standup");
    sim.drag(grab, grab + Vec2::new(0.0, layout::HOUR_H * 22.0 / 60.0)); // 22 minutes down
    assert_eq!(sim.ev("standup"), (t("2026-11-10 09:15"), t("2026-11-10 09:45")));
}

#[test]
fn event_crossing_midnight_can_be_grabbed_by_its_second_day() {
    let mut sim = Sim::week(vec![event("party", "2026-11-10 22:00", "2026-11-11 02:00")]);
    // The Wed piece is drawn after the Tue piece; grab it explicitly.
    let wed = sim.at("2026-11-11", "01:00");
    sim.drag(wed, wed + Vec2::new(0.0, layout::HOUR_H));
    assert_eq!(sim.ev("party"), (t("2026-11-10 23:00"), t("2026-11-11 03:00")));
}

#[test]
fn bottom_edge_resizes() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 09:30")]);
    let grip = sim.resize_grip("standup");
    assert_eq!(sim.pointer.cursor(sim.layout().hits.hit(grip)), Some(Cursor::ResizeVertical));
    let out = sim.drag(grip, grip + Vec2::new(0.0, layout::HOUR_H));
    assert_eq!(sim.ev("standup"), (t("2026-11-10 09:00"), t("2026-11-10 10:30")));
    assert_eq!(out, vec![Action::Committed("standup".into())]);
}

#[test]
fn resize_keeps_at_least_fifteen_minutes() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 09:30")]);
    let grip = sim.resize_grip("standup");
    sim.drag(grip, grip - Vec2::new(0.0, 3.0 * layout::HOUR_H));
    assert_eq!(sim.ev("standup"), (t("2026-11-10 09:00"), t("2026-11-10 09:15")));
}

#[test]
fn read_only_events_do_not_drag_but_still_select() {
    let mut sim = Sim::week(vec![event("holiday", "2026-11-10 09:00", "2026-11-10 10:00")]);
    sim.read_only.push("holiday".into());
    let grab = sim.body("holiday");
    let out = sim.drag(grab, grab + Vec2::new(100.0, 100.0));
    assert_eq!(sim.ev("holiday"), (t("2026-11-10 09:00"), t("2026-11-10 10:00")));
    assert!(out.is_empty());
    assert_eq!(sim.edits(), 0);
    assert_eq!(sim.click(grab), vec![Action::Select(Some("holiday".into()))]);
}

#[test]
fn small_wobble_is_still_a_click() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 10:00")]);
    let p = sim.body("standup");
    sim.time += 1.0;
    sim.press(p);
    sim.move_to(p + Vec2::new(3.0, 2.0));
    assert_eq!(sim.release(), vec![Action::Select(Some("standup".into()))]);
    assert_eq!(sim.edits(), 0);
}

#[test]
fn renamed_event_commits_under_its_new_id() {
    let mut sim = Sim::week(vec![event("local:1", "2026-11-10 09:00", "2026-11-10 10:00")]);
    let grab = sim.body("local:1");
    sim.press(grab);
    sim.move_to(grab + Vec2::new(0.0, 48.0));
    assert_eq!(sim.pointer.dragging_id(), Some("local:1"));
    sim.pointer.rename("local:1", "g:abc");
    sim.events[0].id = "g:abc".into();
    assert_eq!(sim.release(), vec![Action::Committed("g:abc".into())]);
}

// ---------------------------------------------------------------------------
// Week view: creating, clicking

#[test]
fn drag_on_empty_grid_creates_an_event() {
    let mut sim = Sim::week(vec![]);
    let from = sim.at("2026-11-11", "14:00");
    sim.press(from);
    sim.move_to(sim.at("2026-11-11", "15:30"));
    assert_eq!(sim.pointer.ghost(), Some((t("2026-11-11 14:00"), t("2026-11-11 15:30"))));
    let out = sim.release();
    assert_eq!(out, vec![Action::New { start: t("2026-11-11 14:00"), end: t("2026-11-11 15:30"), all_day: false }]);
}

#[test]
fn drag_create_upwards_stays_on_the_day() {
    let mut sim = Sim::week(vec![]);
    let out = sim.drag(sim.at("2026-11-11", "14:00"), sim.at("2026-11-11", "11:00"));
    assert_eq!(out, vec![Action::New { start: t("2026-11-11 11:00"), end: t("2026-11-11 14:00"), all_day: false }]);
}

#[test]
fn drag_create_onto_the_next_day_runs_to_that_time() {
    let mut sim = Sim::week(vec![]);
    let from = sim.at("2026-11-11", "14:00");
    sim.press(from);
    sim.move_to(sim.at("2026-11-12", "11:00"));
    assert_eq!(sim.pointer.ghost(), Some((t("2026-11-11 14:00"), t("2026-11-12 11:00"))));
    let out = sim.release();
    assert_eq!(out, vec![Action::New { start: t("2026-11-11 14:00"), end: t("2026-11-12 11:00"), all_day: false }]);
}

#[test]
fn drag_create_back_onto_an_earlier_day_starts_there() {
    let mut sim = Sim::week(vec![]);
    let out = sim.drag(sim.at("2026-11-13", "11:00"), sim.at("2026-11-11", "14:00"));
    assert_eq!(out, vec![Action::New { start: t("2026-11-11 14:00"), end: t("2026-11-13 11:00"), all_day: false }]);
}

#[test]
fn tiny_drag_create_makes_an_hour() {
    let mut sim = Sim::week(vec![]);
    let from = sim.at("2026-11-11", "14:00");
    let out = sim.drag(from, from + Vec2::new(8.0, 0.0));
    assert_eq!(out, vec![Action::New { start: t("2026-11-11 14:00"), end: t("2026-11-11 15:00"), all_day: false }]);
}

#[test]
fn click_selects_and_double_click_opens() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 10:00")]);
    let p = sim.body("standup");
    assert_eq!(sim.click(p), vec![Action::Select(Some("standup".into()))]);
    assert_eq!(sim.double_click(p), vec![Action::Open("standup".into())]);
    assert_eq!(sim.click(sim.at("2026-11-12", "12:00")), vec![Action::Select(None)]);
}

#[test]
fn slow_second_click_is_not_a_double_click() {
    let mut sim = Sim::week(vec![event("standup", "2026-11-10 09:00", "2026-11-10 10:00")]);
    let p = sim.body("standup");
    sim.click(p);
    sim.time += 0.5;
    sim.press(p);
    assert_eq!(sim.release(), vec![Action::Select(Some("standup".into()))]);
}

#[test]
fn double_click_empty_grid_creates_at_the_half_hour() {
    let mut sim = Sim::week(vec![]);
    let out = sim.double_click(sim.at("2026-11-11", "14:40"));
    assert_eq!(out, vec![Action::New { start: t("2026-11-11 14:30"), end: t("2026-11-11 15:30"), all_day: false }]);
}

#[test]
fn double_click_all_day_lane_creates_all_day_event() {
    let mut sim = Sim::week(vec![]);
    let lane = sim.layout().rects.allday;
    let out = sim.double_click(sim.column("2026-11-13", lane.center().y));
    assert_eq!(out, vec![Action::New { start: t("2026-11-13 00:00"), end: t("2026-11-14 00:00"), all_day: true }]);
}

#[test]
fn drag_across_the_all_day_lane_creates_an_all_day_event() {
    let mut sim = Sim::week(vec![]);
    let y = sim.layout().rects.allday.center().y;
    sim.press(sim.column("2026-11-12", y));
    sim.move_to(sim.column("2026-11-10", y));
    assert_eq!(sim.pointer.ghost_days(), Some((d("2026-11-10"), d("2026-11-12"))));
    let out = sim.release();
    assert_eq!(out, vec![Action::New { start: t("2026-11-10 00:00"), end: t("2026-11-13 00:00"), all_day: true }]);
}

#[test]
fn double_click_day_header_opens_day_view() {
    let mut sim = Sim::week(vec![]);
    let out = sim.double_click(sim.column("2026-11-10", 10.0));
    assert_eq!(out, vec![Action::GoTo(d("2026-11-10"), View::Day)]);
}

#[test]
fn scrolled_out_events_are_not_hit() {
    let mut sim = Sim::week(vec![event("early", "2026-11-10 06:00", "2026-11-10 07:00")]);
    let g = sim.layout().geom;
    // Scrolled so the grid is visible from 08:00 down.
    sim.grid_clip = Some(Rect::from_x_y_ranges(0.0..=WIDTH, g.y_of(8 * 60)..=GRID_TOP + 2000.0));
    let p = sim.body("early");
    assert_eq!(sim.layout().hits.hit(p), None);
    assert!(sim.drag(p, p + Vec2::new(0.0, 200.0)).is_empty());
}

// ---------------------------------------------------------------------------
// Month view

fn month_hits(events: &[Event]) -> layout::MonthLayout {
    month_layout(events, true)
}

fn month_layout(events: &[Event], editable: bool) -> layout::MonthLayout {
    let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 626.0));
    layout::month(events, d("2026-11-14"), rect, &|_| 14.0, &|_| editable)
}

/// An empty spot in a month cell (below its day number).
fn month_blank(lay: &layout::MonthLayout, day: &str) -> Pos2 {
    lay.cells.iter().find(|c| c.day == d(day)).unwrap().rect.center() + Vec2::new(0.0, 20.0)
}

#[test]
fn month_day_number_opens_day_and_cell_double_click_creates() {
    let lay = month_hits(&[]);
    let cell = lay.cells.iter().find(|c| c.day == d("2026-11-19")).unwrap();
    let mut p = Pointer::default();
    let click = |p: &mut Pointer, pos, time| {
        let mut out = p.handle(Input::Press { pos }, &lay.hits);
        out.extend(p.handle(Input::Release { pos, time: time + 0.05 }, &lay.hits));
        out
    };
    assert_eq!(click(&mut p, cell.number.center(), 1.0), vec![Action::Select(None), Action::GoTo(d("2026-11-19"), View::Day)]);
    let blank = cell.rect.center() + Vec2::new(0.0, 20.0);
    click(&mut p, blank, 5.0);
    assert_eq!(
        click(&mut p, blank, 5.2),
        vec![Action::New { start: t("2026-11-19 09:00"), end: t("2026-11-19 10:00"), all_day: false }]
    );
}

#[test]
fn month_drag_across_empty_days_creates_an_all_day_event() {
    let lay = month_hits(&[]);
    let mut p = Pointer::default();
    p.handle(Input::Press { pos: month_blank(&lay, "2026-11-18") }, &lay.hits);
    // Across a week boundary: Wed 18th to Mon 23rd.
    assert_eq!(p.handle(Input::Move { pos: month_blank(&lay, "2026-11-23") }, &lay.hits), vec![]);
    assert_eq!(p.ghost_days(), Some((d("2026-11-18"), d("2026-11-23"))));
    let out = p.handle(Input::Release { pos: month_blank(&lay, "2026-11-23"), time: 1.0 }, &lay.hits);
    assert_eq!(out, vec![Action::New { start: t("2026-11-18 00:00"), end: t("2026-11-24 00:00"), all_day: true }]);
}

#[test]
fn month_events_drag_to_another_day_keeping_their_times() {
    let events = vec![event("standup", "2026-11-10 09:00", "2026-11-10 10:00")];
    let lay = month_hits(&events);
    let r = lay.rows[0].rect;
    let mut p = Pointer::default();
    p.handle(Input::Press { pos: r.center() }, &lay.hits);
    let out = p.handle(Input::Move { pos: month_blank(&lay, "2026-11-19") }, &lay.hits);
    assert_eq!(
        out,
        vec![
            Action::Select(Some("standup".into())),
            Action::BeginEdit("standup".into()),
            Action::SetTimes { id: "standup".into(), start: t("2026-11-19 09:00"), end: t("2026-11-19 10:00") },
        ]
    );
    let out = p.handle(Input::Release { pos: month_blank(&lay, "2026-11-19"), time: 1.0 }, &lay.hits);
    assert_eq!(out, vec![Action::Committed("standup".into())]);
}

#[test]
fn month_all_day_events_drag_by_the_day_grabbed() {
    // Grabbed on its second day, dropped on the 20th: it now starts the 19th.
    let events = vec![all_day("trip", "2026-11-10", "2026-11-13")];
    let lay = month_hits(&events);
    let r = lay.banners[0].rect;
    let grab = Pos2::new(r.left() + lay.cell_w * 1.5, r.center().y);
    let mut p = Pointer::default();
    p.handle(Input::Press { pos: grab }, &lay.hits);
    let out = p.handle(Input::Move { pos: month_blank(&lay, "2026-11-20") }, &lay.hits);
    assert_eq!(out.last(), Some(&Action::SetTimes { id: "trip".into(), start: t("2026-11-19 00:00"), end: t("2026-11-22 00:00") }));
}

#[test]
fn read_only_month_events_select_but_do_not_drag() {
    let events = vec![event("standup", "2026-11-10 09:00", "2026-11-10 10:00")];
    let lay = month_layout(&events, false);
    let r = lay.rows[0].rect;
    let mut p = Pointer::default();
    p.handle(Input::Press { pos: r.center() }, &lay.hits);
    assert_eq!(p.handle(Input::Move { pos: r.center() + Vec2::new(150.0, 0.0) }, &lay.hits), vec![]);
    assert_eq!(p.handle(Input::Release { pos: r.center(), time: 1.1 }, &lay.hits), vec![]);
    p.handle(Input::Press { pos: r.center() }, &lay.hits);
    assert_eq!(p.handle(Input::Release { pos: r.center(), time: 3.05 }, &lay.hits), vec![Action::Select(Some("standup".into()))]);
}

#[test]
fn month_overflow_shows_more_link() {
    let events: Vec<Event> =
        (0..12).map(|i| event(&format!("e{i}"), &format!("2026-11-10 {:02}:00", 8 + i), &format!("2026-11-10 {:02}:30", 8 + i))).collect();
    let lay = month_hits(&events);
    let &(day, n, r) = lay.more.first().expect("a '+N more' label");
    assert_eq!(day, d("2026-11-10"));
    assert_eq!(n + lay.rows.iter().filter(|p| events[p.ev].start.date() == day).count(), 12);
    let mut p = Pointer::default();
    p.handle(Input::Press { pos: r.center() }, &lay.hits);
    assert_eq!(p.cursor(lay.hits.hit(r.center())), None); // pressed, not idle
    assert_eq!(p.handle(Input::Release { pos: r.center(), time: 1.05 }, &lay.hits), vec![Action::GoTo(day, View::Day)]);
}

#[test]
fn clicks_go_to_the_event_drawn_on_top() {
    // "colour" starts 30 min into "nvidia", so it's drawn indented on top.
    let mut sim = Sim::week(vec![
        event("nvidia", "2026-11-09 17:00", "2026-11-09 18:00"),
        event("colour", "2026-11-09 17:30", "2026-11-09 18:00"),
    ]);
    let below = sim.hit_rect("nvidia", |g| g != Grip::Resize);
    let above = sim.hit_rect("colour", |g| g != Grip::Resize);
    assert!(above.left() > below.left() && above.right() >= below.right() - 0.5, "indented on top");

    // Where the top event covers the bottom one, the top one is hit.
    let covered = Pos2::new(above.center().x, above.top() + 6.0);
    assert_eq!(sim.click(covered), vec![Action::Select(Some("colour".into()))]);
    // The bottom event's exposed left strip still belongs to it.
    let strip = Pos2::new(below.left() + 3.0, above.top() + 6.0);
    assert_eq!(sim.click(strip), vec![Action::Select(Some("nvidia".into()))]);
    // Above the top event, the bottom one is hit across its full width.
    let upper = Pos2::new(above.center().x, below.top() + 6.0);
    assert_eq!(sim.click(upper), vec![Action::Select(Some("nvidia".into()))]);
}

// ---------------------------------------------------------------------------
// Right-click menus

#[test]
fn right_click_on_an_event_selects_it_and_opens_its_menu() {
    let mut sim = Sim::week(vec![event("lunch", "2026-11-10 11:45", "2026-11-10 12:30")]);
    let pos = sim.body("lunch");
    let out = sim.send(Input::ContextPress { pos });
    assert_eq!(
        out,
        vec![Action::Select(Some("lunch".into())), Action::ContextMenu { at: MenuAt::Event("lunch".into()), pos }]
    );
}

#[test]
fn right_click_works_on_read_only_events_too() {
    let mut sim = Sim::week(vec![event("holiday", "2026-11-10 09:00", "2026-11-10 10:00")]);
    sim.read_only.push("holiday".into());
    let pos = sim.body("holiday");
    let out = sim.send(Input::ContextPress { pos });
    assert_eq!(out.last(), Some(&Action::ContextMenu { at: MenuAt::Event("holiday".into()), pos }));
}

#[test]
fn right_click_on_empty_space_offers_that_time_or_day() {
    let mut sim = Sim::week(vec![]);
    // The hour grid: the time under the pointer, to the quarter hour.
    let pos = sim.at("2026-11-11", "14:20");
    let out = sim.send(Input::ContextPress { pos });
    assert_eq!(out, vec![Action::Select(None), Action::ContextMenu { at: MenuAt::Time(t("2026-11-11 14:15")), pos }]);
    // The all-day lane: that day, all-day.
    let lane = sim.layout().rects.allday;
    let pos = Pos2::new(sim.column("2026-11-12", 0.0).x, lane.center().y);
    let out = sim.send(Input::ContextPress { pos });
    assert_eq!(out.last(), Some(&Action::ContextMenu { at: MenuAt::Day { day: d("2026-11-12"), all_day: true }, pos }));
}

#[test]
fn right_click_in_the_month_view() {
    let events = vec![event("standup", "2026-11-10 09:00", "2026-11-10 10:00")];
    let lay = month_hits(&events);
    let mut p = Pointer::default();
    let pos = lay.rows[0].rect.center();
    assert_eq!(
        p.handle(Input::ContextPress { pos }, &lay.hits),
        vec![Action::Select(Some("standup".into())), Action::ContextMenu { at: MenuAt::Event("standup".into()), pos }]
    );
    let pos = month_blank(&lay, "2026-11-19");
    assert_eq!(
        p.handle(Input::ContextPress { pos }, &lay.hits),
        vec![Action::Select(None), Action::ContextMenu { at: MenuAt::Day { day: d("2026-11-19"), all_day: false }, pos }]
    );
}

#[test]
fn right_click_during_a_drag_is_ignored() {
    let mut sim = Sim::week(vec![event("lunch", "2026-11-10 11:45", "2026-11-10 12:30")]);
    let grab = sim.body("lunch");
    sim.press(grab);
    sim.move_to(grab + Vec2::new(0.0, 30.0));
    assert_eq!(sim.send(Input::ContextPress { pos: grab }), vec![]);
    assert_eq!(sim.release(), vec![Action::Committed("lunch".into())]);
}

#[test]
fn right_click_breaks_up_a_double_click() {
    let mut sim = Sim::week(vec![event("lunch", "2026-11-10 11:45", "2026-11-10 12:30")]);
    let pos = sim.body("lunch");
    sim.click(pos);
    sim.send(Input::ContextPress { pos });
    sim.press(pos);
    assert_eq!(sim.release(), vec![Action::Select(Some("lunch".into()))], "not an Open");
}
