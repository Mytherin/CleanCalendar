//! Pure geometry for the event views: where every event, cell, and lane goes,
//! and what each part of the screen means to the pointer (the hit map).
//!
//! Nothing in here touches egui's `Context` or paints anything, so layouts can
//! be built in plain unit tests and fed to the interaction state machine in
//! `interact.rs`. `views.rs` paints from these same layouts, which keeps what
//! you see and what you can click in agreement.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use eframe::egui::{Pos2, Rect, Vec2};

use crate::dates::*;
use crate::model::{Event, Store};
use crate::recurrence;

/// A typical hour height (macOS Calendar showing 12 hours), for tests. The
/// views size hours to the "Show N hours at a time" setting.
#[cfg(test)]
pub const HOUR_H: f32 = 52.0;
/// Shortest drawn height of a timed event, enough for one line of title.
pub const MIN_EVENT_H: f32 = 18.0;

/// `MIN_EVENT_H` in minutes of a time grid with hours `hour_h` tall.
fn min_event_minutes(hour_h: f32) -> i64 {
    (MIN_EVENT_H / hour_h * 60.0).ceil() as i64
}
pub const GUTTER: f32 = 58.0;
pub const BANNER_H: f32 = 20.0;
/// Gridlines are 1px strokes centered on their coordinate. Events in the
/// week/day grid fill the space between them edge to edge, like Apple
/// Calendar: they start half a line past one gridline and stop half a line
/// before the next, so the gridline is the only separator.
const HALF_LINE: f32 = 0.5;
pub const DAY_HEADER_H: f32 = 34.0;
/// Space above the midnight line in the hour grid.
pub const GRID_PAD: f32 = 6.0;
/// Strip on the right of the hour grid left to the (floating) scroll bar.
pub const SCROLLBAR_W: f32 = 12.0;
/// Height of the resize handle at the bottom of a timed event.
const RESIZE_GRIP: f32 = 6.0;
pub const MONTH_HEADER_H: f32 = 26.0;
const MONTH_ROW_H: f32 = 18.0;
/// Gap between a month cell's right edge and its day number (the weekday
/// header above is right-aligned on the same line).
pub const MONTH_NUMBER_INSET: f32 = 10.0;

// ---------------------------------------------------------------------------
// Hit map

/// What the pointer is over.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Event { id: String, start: NaiveDateTime, end: NaiveDateTime, grip: Grip },
    /// Empty space in the hour grid.
    TimeGrid,
    /// Empty space in the all-day lane.
    AllDayLane,
    DayHeader(NaiveDate),
    MonthCell(NaiveDate),
    MonthDayNumber(NaiveDate),
    More(NaiveDate),
}

/// What dragging an event from this spot does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grip {
    /// Not draggable (e.g. a read-only calendar).
    None,
    /// Move a timed event in time and across days.
    Move,
    /// Drag the end of a timed event.
    Resize,
    /// Move by whole days, keeping the times: an all-day / multi-day
    /// banner, or any event in the month view.
    MoveDays,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub rect: Rect,
    /// Only the part of `rect` inside `clip` is live (e.g. the scrolled
    /// hour grid is hidden under the all-day lane).
    pub clip: Rect,
    pub target: Target,
}

/// Everything the interaction state machine needs to know about a view.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    /// Back to front: later hits sit on top of earlier ones.
    pub hits: Vec<Hit>,
    /// Day/week views: maps positions to times.
    pub grid: Option<TimeGrid>,
}

impl HitMap {
    pub fn hit(&self, pos: Pos2) -> Option<&Target> {
        self.hits.iter().rev().find(|h| h.clip.contains(pos) && h.rect.contains(pos)).map(|h| &h.target)
    }

    /// The day under `pos`, for drags by whole days: a day/week column, or
    /// a month cell (whatever is drawn on top of it). None between cells.
    pub fn day_at(&self, pos: Pos2) -> Option<NaiveDate> {
        if let Some(g) = self.grid {
            return Some(g.day_at(pos.x));
        }
        self.hits.iter().find_map(|h| match h.target {
            Target::MonthCell(day) if h.rect.contains(pos) => Some(day),
            _ => None,
        })
    }

    fn push(&mut self, rect: Rect, clip: Rect, target: Target) {
        self.hits.push(Hit { rect, clip, target });
    }
}

/// Column / hour geometry of a day or week view.
#[derive(Clone, Copy, Debug)]
pub struct TimeGrid {
    pub first: NaiveDate,
    pub ndays: i64,
    /// x of the first day column's left edge.
    pub left: f32,
    pub col_w: f32,
    /// y of the midnight line.
    pub top: f32,
    /// Height of one hour.
    pub hour_h: f32,
}

impl TimeGrid {
    pub fn col_x(&self, i: i64) -> f32 {
        self.left + i as f32 * self.col_w
    }

    pub fn day_at(&self, x: f32) -> NaiveDate {
        let i = (((x - self.left) / self.col_w).floor() as i64).clamp(0, self.ndays - 1);
        self.first + Duration::days(i)
    }

    pub fn time_at(&self, pos: Pos2) -> NaiveDateTime {
        let mins = (((pos.y - self.top) / self.hour_h) * 60.0).clamp(0.0, 24.0 * 60.0 - 1.0) as i64;
        midnight(self.day_at(pos.x)) + Duration::minutes(mins)
    }

    pub fn y_of(&self, minutes: i64) -> f32 {
        self.top + minutes as f32 / 60.0 * self.hour_h
    }
}

/// An event placed on screen; `ev` indexes the events the layout was built from.
#[derive(Clone, Copy, Debug)]
pub struct Placed {
    pub ev: usize,
    pub rect: Rect,
}

// ---------------------------------------------------------------------------
// Shared

/// Visible events overlapping [first, first + n days), longest first per start.
pub fn events_in(store: &Store, first: NaiveDate, n: i64) -> Vec<Event> {
    let a = midnight(first);
    let b = midnight(first + Duration::days(n));
    let visible = store.events.iter().filter(|e| store.is_visible(&e.calendar_id));
    let mut v: Vec<Event> = visible
        .clone()
        .filter(|e| e.recurrence.is_none())
        .filter(|e| e.start < b && (e.end > a || (e.end <= e.start && e.start >= a)))
        .cloned()
        .collect();
    // Repeating events show as their occurrences.
    for series in visible.filter(|e| e.recurrence.is_some() && e.start < b) {
        v.extend(recurrence::occurrences(series, a, b));
    }
    v.sort_by(|x, y| x.start.cmp(&y.start).then(y.duration().cmp(&x.duration())));
    v
}

/// Packs banner (all-day / multi-day) events into lanes across a run of days.
/// Returns (event index, first column, last column, lane).
pub fn banner_lanes(events: &[Event], first: NaiveDate, ndays: i64) -> Vec<(usize, i64, i64, usize)> {
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

fn event_target(ev: &Event, grip: Grip) -> Target {
    Target::Event { id: ev.id.clone(), start: ev.start, end: ev.end, grip }
}

// ---------------------------------------------------------------------------
// Day / week

/// Screen areas egui allocated for a day/week view.
#[derive(Clone, Copy, Debug)]
pub struct WeekRects {
    pub header: Rect,
    pub allday: Rect,
    /// The full (scrollable) hour grid.
    pub grid: Rect,
    /// The visible part of the hour grid.
    pub grid_clip: Rect,
    /// Height of one hour in `grid` (see [`grid_height`]).
    pub hour_h: f32,
}

pub struct WeekLayout {
    pub first: NaiveDate,
    pub ndays: i64,
    pub geom: TimeGrid,
    pub rects: WeekRects,
    pub banners: Vec<Placed>,
    pub timed: Vec<Placed>,
    pub hits: HitMap,
}

/// Height of the all-day lane for these events.
pub fn allday_height(events: &[Event], first: NaiveDate, ndays: i64) -> f32 {
    let n = banner_lanes(events, first, ndays).iter().map(|l| l.3 + 1).max().unwrap_or(0).max(1);
    n as f32 * BANNER_H + 6.0
}

/// Total height of the scrollable hour grid.
pub fn grid_height(hour_h: f32) -> f32 {
    24.0 * hour_h + 2.0 * GRID_PAD
}

pub fn week(
    events: &[Event],
    first: NaiveDate,
    ndays: i64,
    rects: WeekRects,
    editable: &dyn Fn(&Event) -> bool,
) -> WeekLayout {
    let col_w = (rects.grid.width() - GUTTER) / ndays as f32;
    let geom = TimeGrid {
        first,
        ndays,
        left: rects.grid.left() + GUTTER,
        col_w,
        top: rects.grid.top() + GRID_PAD,
        hour_h: rects.hour_h,
    };
    let mut hits = HitMap { hits: Vec::new(), grid: Some(geom) };

    // Day headers (double-click to open that day).
    if ndays > 1 {
        for i in 0..ndays {
            let cell = Rect::from_min_size(Pos2::new(geom.col_x(i), rects.header.top()), Vec2::new(col_w, rects.header.height()));
            hits.push(cell, rects.header, Target::DayHeader(first + Duration::days(i)));
        }
    }

    // All-day lane and its banners.
    let lane_area = Rect::from_x_y_ranges(geom.left..=rects.allday.right(), rects.allday.y_range());
    hits.push(lane_area, rects.allday, Target::AllDayLane);
    let mut banners = Vec::new();
    for (i, c0, c1, lane) in banner_lanes(events, first, ndays) {
        let ev = &events[i];
        // Lanes are BANNER_H apart with 2px between bars, inside a lane area
        // 6px taller than the bars: 4px above the first, 4px below the last.
        let y = rects.allday.top() + 4.0 + lane as f32 * BANNER_H;
        let rect = Rect::from_min_max(
            Pos2::new(geom.col_x(c0) + HALF_LINE, y),
            Pos2::new(geom.col_x(c1 + 1) - HALF_LINE, y + BANNER_H - 2.0),
        );
        banners.push(Placed { ev: i, rect });
        let grip = if editable(ev) { Grip::MoveDays } else { Grip::None };
        hits.push(rect, rects.allday, event_target(ev, grip));
    }

    // Hour grid and timed events, placed like macOS Calendar (see layout_day).
    let mut clip = rects.grid_clip;
    clip.max.x -= SCROLLBAR_W;
    let grid_area = Rect::from_x_y_ranges(geom.left..=rects.grid.right(), rects.grid.y_range());
    hits.push(grid_area, clip, Target::TimeGrid);
    let timed_idx: Vec<usize> = (0..events.len()).filter(|&i| !events[i].is_banner()).collect();
    let mut timed = Vec::new();
    for d in 0..ndays {
        // Edge to edge between the gridlines.
        let left = geom.col_x(d) + HALF_LINE;
        for seg in layout_day(events, &timed_idx, first + Duration::days(d), col_w - 2.0 * HALF_LINE, rects.hour_h) {
            let ev = &events[seg.ev];
            let x0 = left + seg.x;
            let w = seg.w;
            let y0 = geom.y_of(seg.start_min) + HALF_LINE;
            let y1 = (geom.y_of(seg.end_min) - HALF_LINE).max(y0 + MIN_EVENT_H);
            let rect = Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x0 + w, y1));
            timed.push(Placed { ev: seg.ev, rect });
            if editable(ev) {
                hits.push(rect, clip, event_target(ev, Grip::Move));
                if seg.ends_here {
                    let grip = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - RESIZE_GRIP), rect.max);
                    hits.push(grip, clip, event_target(ev, Grip::Resize));
                }
            } else {
                hits.push(rect, clip, event_target(ev, Grip::None));
            }
        }
    }

    WeekLayout { first, ndays, geom, rects, banners, timed, hits }
}

/// Where the event block `placed[i]` starts being hidden under a block drawn
/// after it (on top), e.g. "Colour consultation" at 17:30 nested inside
/// "Nvidia sync" 17:00–18:00; its own bottom if nothing covers it. Its text
/// should stop above that.
pub fn covered_from(placed: &[Placed], i: usize) -> f32 {
    let r = placed[i].rect;
    placed[i + 1..]
        .iter()
        .map(|o| o.rect)
        .filter(|o| o.left() < r.right() && o.right() > r.left() && o.top() >= r.top() && o.top() < r.bottom())
        .map(|o| o.top())
        .fold(r.bottom(), f32::min)
}

#[derive(Debug)]
pub struct Segment {
    pub ev: usize,
    pub start_min: i64,
    pub end_min: i64,
    /// Horizontal position within the day column, in px from its left edge.
    pub x: f32,
    pub w: f32,
    /// Whether the event ends on this day (so it can be resized here).
    pub ends_here: bool,
}

/// Overlapping events that start less than this many minutes apart share the
/// column side by side; a later one is drawn on top, indented.
const SIDE_BY_SIDE_MIN: i64 = 30;
/// How far an event drawn on top of an earlier one is indented.
const NEST_INDENT: f32 = 8.0;

/// Clips the events `idx` (into `events`) to `day` and places overlapping
/// ones the way macOS Calendar does, in a day column `width` px wide:
///
/// - events that start close together (< 30 min apart) split the space
///   side by side;
/// - an event that starts later inside another is drawn on top of it,
///   indented a little from that event's left edge and running to the
///   column's right edge.
///
/// Segments come back in drawing order (later ones on top), which is also
/// the order their hit areas must be registered in.
pub fn layout_day(events: &[Event], idx: &[usize], day: NaiveDate, width: f32, hour_h: f32) -> Vec<Segment> {
    let ds = midnight(day);
    let de = ds + Duration::days(1);
    let mut segs: Vec<Segment> = idx
        .iter()
        .map(|&i| (i, &events[i]))
        .filter(|(_, e)| e.occurs_on(day))
        .map(|(i, e)| {
            let s = e.start.max(ds);
            let end = e.end.min(de);
            let start_min = (s - ds).num_minutes();
            // Short events are drawn taller than their duration so the title
            // fits; lay them out at that drawn length, so the next event goes
            // beside them instead of underneath their text.
            let end_min = ((end - ds).num_minutes()).max(start_min + min_event_minutes(hour_h));
            Segment { ev: i, start_min, end_min, x: 0.0, w: width, ends_here: e.end <= de }
        })
        .collect();
    segs.sort_by_key(|s| (s.start_min, -s.end_min));

    // A group is a horizontal span shared side by side by its members.
    struct Group {
        x0: f32,
        x1: f32,
        members: Vec<usize>,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut group_of: Vec<usize> = Vec::with_capacity(segs.len());
    let place = |g: &Group, k: usize| {
        let w = (g.x1 - g.x0) / g.members.len() as f32;
        (g.x0 + k as f32 * w, w)
    };

    for i in 0..segs.len() {
        // The topmost already-placed event this one overlaps, if any.
        let under = (0..i).rev().find(|&j| segs[j].start_min < segs[i].end_min && segs[i].start_min < segs[j].end_min);
        let g = match under {
            None => {
                groups.push(Group { x0: 0.0, x1: width, members: vec![i] });
                groups.len() - 1
            }
            Some(j) if segs[i].start_min - segs[j].start_min < SIDE_BY_SIDE_MIN => {
                // Starts with it: share its span side by side.
                let g = group_of[j];
                groups[g].members.push(i);
                g
            }
            Some(j) => {
                // Starts later: on top, indented from the event below.
                let (x, _) = place(&groups[group_of[j]], groups[group_of[j]].members.iter().position(|&m| m == j).unwrap());
                let x0 = (x + NEST_INDENT).min(width * 0.6);
                groups.push(Group { x0, x1: width, members: vec![i] });
                groups.len() - 1
            }
        };
        group_of.push(g);
    }
    for g in &groups {
        for (k, &m) in g.members.iter().enumerate() {
            let (x, w) = place(g, k);
            segs[m].x = x;
            segs[m].w = w;
        }
    }
    segs
}

// ---------------------------------------------------------------------------
// Month

pub struct MonthCell {
    pub day: NaiveDate,
    pub rect: Rect,
    /// The day number (click to open that day).
    pub number: Rect,
    pub in_month: bool,
}

pub struct MonthLayout {
    pub header_h: f32,
    pub cell_w: f32,
    pub cells: Vec<MonthCell>,
    pub banners: Vec<Placed>,
    /// Timed events listed in day cells.
    pub rows: Vec<Placed>,
    /// "N more" labels: (day, count, rect).
    pub more: Vec<(NaiveDate, usize, Rect)>,
    pub hits: HitMap,
}

/// The first day shown in the month grid containing `month`, and how many weeks it shows.
pub fn month_span(month: NaiveDate) -> (NaiveDate, i64) {
    let first = first_of_month(month);
    let grid_start = start_of_week(first);
    let last = first + Duration::days(days_in_month(first) as i64 - 1);
    (grid_start, (last - grid_start).num_days() / 7 + 1)
}

/// Where the month view is scrolled to. Like Apple Calendar, it scrolls
/// smoothly through weeks rather than paging by months.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonthScroll {
    /// The first day of the week in the top row.
    pub top: NaiveDate,
    /// How much of the top row (0..1) has scrolled up out of view.
    pub offset: f32,
    /// How many rows fit the view (kept while scrolling, so rows don't
    /// change height under the pointer).
    pub rows: i64,
}

impl MonthScroll {
    /// The month containing `month`, its first week at the top.
    pub fn at_rest(month: NaiveDate) -> Self {
        let (top, rows) = month_span(month);
        MonthScroll { top, offset: 0.0, rows }
    }

    /// Scrolls by `rows` (positive is later).
    pub fn scroll(&mut self, rows: f32) {
        // A four-week month at rest (a February starting on the first day
        // of the week) would otherwise make every month scrolled to show in
        // four tall rows; scrolling shows at least five, like the others.
        self.rows = self.rows.max(5);
        self.offset += rows;
        let whole = self.offset.floor();
        // Weeks out of chrono's range (scrolling for millennia) stop there.
        match Duration::try_days(7 * whole as i64).and_then(|d| self.top.checked_add_signed(d)) {
            Some(top) => {
                self.top = top;
                self.offset -= whole;
            }
            None => self.offset = 0.0,
        }
    }

    /// The month shown (the one in the middle row), first day.
    pub fn month(&self) -> NaiveDate {
        let mid = (self.offset + self.rows as f32 / 2.0).floor() as i64;
        first_of_month(self.top + Duration::days(7 * mid + 3))
    }

    /// The first day drawn, and how many weeks are (partly) visible.
    pub fn span(&self) -> (NaiveDate, i64) {
        (self.top, self.rows + (self.offset > 0.0) as i64)
    }

    /// Moves `t` (0..1) of the way to the nearest week boundary; returns
    /// whether it still has some way to go.
    pub fn settle(&mut self, t: f32) -> bool {
        let target = self.offset.round();
        if (target - self.offset).abs() < 0.002 {
            if let Some(top) = self.top.checked_add_signed(Duration::days(7 * target as i64)) {
                self.top = top;
            }
            self.offset = 0.0;
            return false;
        }
        self.scroll((target - self.offset) * t);
        true
    }
}

/// Lays out the month view scrolled to `pos` in `rect`. `number_w` measures
/// a day number label (painting decides the font).
pub fn month(
    events: &[Event],
    pos: MonthScroll,
    rect: Rect,
    number_w: &dyn Fn(NaiveDate) -> f32,
    editable: &dyn Fn(&Event) -> bool,
) -> MonthLayout {
    // Events move by whole days in the month grid, keeping their times.
    let grip = |ev: &Event| if editable(ev) { Grip::MoveDays } else { Grip::None };
    let (grid_start, weeks) = pos.span();
    let month = pos.month();
    let header_h = MONTH_HEADER_H;
    // Rows scroll under the weekday header.
    let grid = Rect::from_min_max(Pos2::new(rect.left(), rect.top() + header_h), rect.max);
    let cell_w = rect.width() / 7.0;
    let cell_h = grid.height() / pos.rows as f32;
    let row_h = MONTH_ROW_H;
    let max_rows = (((cell_h - 30.0) / row_h).floor() as usize).max(1);
    let mut out = MonthLayout {
        header_h,
        cell_w,
        cells: Vec::new(),
        banners: Vec::new(),
        rows: Vec::new(),
        more: Vec::new(),
        hits: HitMap::default(),
    };

    for w in 0..weeks {
        let week_start = grid_start + Duration::days(w * 7);
        let top = grid.top() + (w as f32 - pos.offset) * cell_h;

        for d in 0..7 {
            let day = week_start + Duration::days(d);
            let cell = Rect::from_min_size(Pos2::new(rect.left() + d as f32 * cell_w, top), Vec2::new(cell_w, cell_h));
            let nw = number_w(day);
            let number = Rect::from_center_size(
                Pos2::new(cell.right() - MONTH_NUMBER_INSET - nw / 2.0, cell.top() + 14.0),
                Vec2::new(nw + 12.0, 22.0),
            );
            out.hits.push(cell, grid, Target::MonthCell(day));
            out.hits.push(number, grid, Target::MonthDayNumber(day));
            out.cells.push(MonthCell { day, rect: cell, number, in_month: day.month() == month.month() });
        }

        // Banners spanning across the week row.
        let lanes = banner_lanes(events, week_start, 7);
        let timed_on = |d: i64| {
            let day = week_start + Duration::days(d);
            (0..events.len()).filter(|&i| !events[i].is_banner() && events[i].occurs_on(day)).collect::<Vec<usize>>()
        };
        let timed_by_day: Vec<Vec<usize>> = (0..7).map(timed_on).collect();
        let mut used = [0usize; 7];
        for &(_, c0, c1, lane) in &lanes {
            for c in c0..=c1 {
                used[c as usize] = used[c as usize].max(lane + 1);
            }
        }
        // The last row is kept for "N more" where a day has more than fits;
        // a banner may use it only where everything else fits too.
        let fits = |c: i64| used[c as usize] + timed_by_day[c as usize].len() <= max_rows;
        let mut hidden = [0usize; 7];
        for (i, c0, c1, lane) in lanes {
            if lane >= max_rows || (lane + 1 == max_rows && !(c0..=c1).all(fits)) {
                for c in c0..=c1 {
                    hidden[c as usize] += 1;
                }
                continue;
            }
            let r = Rect::from_min_max(
                Pos2::new(rect.left() + c0 as f32 * cell_w + 3.0, top + 28.0 + lane as f32 * row_h),
                Pos2::new(rect.left() + (c1 + 1) as f32 * cell_w - 3.0, top + 28.0 + lane as f32 * row_h + row_h - 2.0),
            );
            out.banners.push(Placed { ev: i, rect: r });
            out.hits.push(r, grid, event_target(&events[i], grip(&events[i])));
        }

        // Timed events in each day cell.
        for d in 0..7 {
            let day = week_start + Duration::days(d);
            let left = rect.left() + d as f32 * cell_w;
            let timed = &timed_by_day[d as usize];
            let mut row = used[d as usize];
            let mut more = hidden[d as usize];
            for (k, &i) in timed.iter().enumerate() {
                let remaining = timed.len() - k;
                if row + 1 >= max_rows && remaining > 1 || row >= max_rows {
                    more += remaining;
                    break;
                }
                let r = Rect::from_min_size(Pos2::new(left + 3.0, top + 28.0 + row as f32 * row_h), Vec2::new(cell_w - 6.0, row_h - 2.0));
                out.rows.push(Placed { ev: i, rect: r });
                out.hits.push(r, grid, event_target(&events[i], grip(&events[i])));
                row += 1;
            }
            if more > 0 {
                // Where the next event row would go.
                let pos = Pos2::new(left + 3.0, top + 28.0 + row.min(max_rows - 1) as f32 * row_h);
                let r = Rect::from_min_size(pos, Vec2::new(cell_w - 6.0, row_h - 2.0));
                out.more.push((day, more, r));
                out.hits.push(r, grid, Target::More(day));
            }
        }
    }
    out
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
            attendees: Vec::new(),
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

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()
    }

    #[test]
    fn events_starting_together_share_the_column() {
        // Like "hairdresser" 14:45 and "Engineering Process Meeting" 15:00.
        let all = [ev("a", (14, 45), (15, 45)), ev("b", (15, 0), (16, 0)), ev("c", (17, 0), (18, 0))];
        let segs = layout_day(&all, &[0, 1, 2], day(), 200.0, HOUR_H);
        let get = |id: &str| segs.iter().find(|s| all[s.ev].id == id).unwrap();
        assert_eq!((get("a").x, get("a").w), (0.0, 100.0));
        assert_eq!((get("b").x, get("b").w), (100.0, 100.0));
        assert_eq!((get("c").x, get("c").w), (0.0, 200.0), "no overlap: full width");
    }

    #[test]
    fn later_overlapping_event_is_indented_on_top() {
        // Like "Colour Consultation" 17:30 inside "Nvidia sync" 17:00–18:00.
        let all = [ev("inner", (17, 30), (18, 0)), ev("outer", (17, 0), (18, 0))];
        let segs = layout_day(&all, &[0, 1], day(), 200.0, HOUR_H);
        let order: Vec<&str> = segs.iter().map(|s| all[s.ev].id.as_str()).collect();
        assert_eq!(order, ["outer", "inner"], "the later one is drawn (and hit) on top");
        assert_eq!((segs[0].x, segs[0].w), (0.0, 200.0), "the one below keeps the full width");
        assert_eq!((segs[1].x, segs[1].x + segs[1].w), (NEST_INDENT, 200.0), "indented, to the right edge");
    }

    fn all_day(id: &str, first: u32, days: i64) -> Event {
        let mut e = ev(id, (0, 0), (0, 0));
        e.start = midnight(NaiveDate::from_ymd_opt(2026, 11, first).unwrap());
        e.end = e.start + Duration::days(days);
        e.all_day = true;
        e
    }

    /// November 2026 (six week rows) in a rect whose cells fit 3 rows.
    fn month_3_rows(events: &[Event]) -> MonthLayout {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, MONTH_HEADER_H + 6.0 * 90.0));
        let lay = month(events, MonthScroll::at_rest(day()), rect, &|_| 14.0, &|_| true);
        let cell_h = 90.0;
        assert_eq!(((cell_h - 30.0) / MONTH_ROW_H).floor() as usize, 3);
        lay
    }

    fn assert_no_overlaps(lay: &MonthLayout) {
        let mut rects: Vec<Rect> = lay.banners.iter().chain(&lay.rows).map(|p| p.rect).collect();
        rects.extend(lay.more.iter().map(|m| m.2));
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.intersects(b.shrink(0.5)), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_banner_in_the_last_row_shows_when_the_day_has_room() {
        // Three all-day events on the 10th and nothing else: all three fit.
        let all = [all_day("a", 10, 1), all_day("b", 10, 1), all_day("c", 10, 1)];
        let lay = month_3_rows(&all);
        assert_eq!(lay.banners.len(), 3);
        assert!(lay.more.is_empty(), "no \"1 more\" in place of the third");
        assert_no_overlaps(&lay);
    }

    #[test]
    fn a_banner_in_the_last_row_folds_into_more_when_the_day_overflows() {
        // Three banners plus a timed event on the 10th: four things, three rows.
        let all = [all_day("a", 10, 1), all_day("b", 10, 1), all_day("c", 10, 1), ev("t", (9, 0), (10, 0))];
        let lay = month_3_rows(&all);
        assert_eq!(lay.banners.len(), 2);
        assert!(lay.rows.is_empty());
        assert_eq!(lay.more.len(), 1);
        assert_eq!(lay.more[0].1, 2, "the hidden banner and the timed event");
        assert_no_overlaps(&lay);
    }

    #[test]
    fn a_long_banner_in_the_last_row_hides_where_any_of_its_days_overflow() {
        // Lanes 0 and 1 on the 9th–10th; "c" lands in lane 2 across 9th–11th,
        // and the 11th also has two timed events, so it doesn't fit there.
        let all = [
            all_day("a", 9, 2),
            all_day("b", 9, 2),
            all_day("c", 9, 3),
            {
                let mut t = ev("t1", (9, 0), (10, 0));
                t.start += Duration::days(1);
                t.end += Duration::days(1);
                t
            },
            {
                let mut t = ev("t2", (11, 0), (12, 0));
                t.start += Duration::days(1);
                t.end += Duration::days(1);
                t
            },
        ];
        let lay = month_3_rows(&all);
        assert!(lay.banners.iter().all(|b| all[b.ev].id != "c"));
        assert_no_overlaps(&lay);
    }

    #[test]
    fn more_label_lines_up_with_the_event_rows() {
        let all: Vec<Event> = (0..6).map(|h| ev(&format!("e{h}"), (8 + h, 0), (8 + h, 30))).collect();
        let lay = month_3_rows(&all);
        assert_eq!(lay.rows.len(), 2);
        let (_, n, more) = lay.more[0];
        assert_eq!(n, 4);
        assert_eq!(more.left(), lay.rows[0].rect.left());
        assert_eq!(more.width(), lay.rows[0].rect.width());
        assert_eq!(more.top() - lay.rows[1].rect.top(), MONTH_ROW_H, "the row after the last event");
        assert_no_overlaps(&lay);
    }

    #[test]
    fn text_stops_where_an_event_on_top_begins() {
        let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1));
        let placed = [
            Placed { ev: 0, rect: rect(0.0, 100.0, 200.0, 152.0) }, // 17:00–18:00
            Placed { ev: 1, rect: rect(8.0, 126.0, 200.0, 152.0) }, // nested at 17:30
            Placed { ev: 2, rect: rect(200.0, 100.0, 400.0, 152.0) }, // next column: no cover
        ];
        assert_eq!(covered_from(&placed, 0), 126.0);
        assert_eq!(covered_from(&placed, 1), 152.0, "nothing on top of the nested one");
        assert_eq!(covered_from(&placed, 2), 152.0, "touching at the edge isn't covering");
    }

    #[test]
    fn nesting_builds_on_the_event_below() {
        // Profile photoshoot 10:30–11:30, koffie 11:15–12:00, lunch 11:50–12:30:
        // koffie nests in the photoshoot; lunch starts 35 min after koffie, so
        // it nests again, one more indent in.
        let all = [ev("shoot", (10, 30), (11, 30)), ev("koffie", (11, 15), (12, 0)), ev("lunch", (11, 50), (12, 30))];
        let segs = layout_day(&all, &[0, 1, 2], day(), 200.0, HOUR_H);
        let x: Vec<f32> = segs.iter().map(|s| s.x).collect();
        assert_eq!(x, [0.0, NEST_INDENT, 2.0 * NEST_INDENT]);
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn scrolling_away_from_a_four_week_month_shows_at_least_five_rows() {
        // February 2027 starts on a Monday, February 2026 on a Sunday: one
        // of them fills exactly four weeks whichever day weeks start on.
        let four = [date(2026, 2, 10), date(2027, 2, 10)]
            .into_iter()
            .map(MonthScroll::at_rest)
            .find(|p| p.rows == 4)
            .expect("a four-week February");
        let mut pos = four;
        pos.scroll(3.5);
        assert_eq!(pos.rows, 5);
        pos.scroll(-3.5);
        assert_eq!(pos.rows, 5, "kept while scrolling, so rows don't change height");
        assert!(MonthScroll::at_rest(date(2026, 11, 10)).rows >= 5, "other months unchanged at rest");
    }

    #[test]
    fn month_scroll_moves_through_weeks() {
        let mut pos = MonthScroll::at_rest(date(2026, 11, 10));
        let top = pos.top;
        assert_eq!(pos.month(), date(2026, 11, 1));
        pos.scroll(0.25);
        assert_eq!((pos.top, pos.offset), (top, 0.25));
        assert_eq!(pos.span(), (top, pos.rows + 1), "a part-visible row at the bottom");
        pos.scroll(1.0);
        assert_eq!((pos.top, pos.offset), (top + Duration::days(7), 0.25));
        pos.scroll(-2.5);
        assert_eq!((pos.top, pos.offset), (top - Duration::days(14), 0.75));
    }

    #[test]
    fn month_scroll_shows_the_month_in_the_middle_row() {
        let mut pos = MonthScroll::at_rest(date(2026, 11, 10));
        for _ in 0..3 {
            pos.scroll(1.0);
        }
        assert_eq!(pos.month(), date(2026, 12, 1), "three weeks on, December fills the middle");
        for _ in 0..6 {
            pos.scroll(-1.0);
        }
        assert_eq!(pos.month(), date(2026, 10, 1));
    }

    #[test]
    fn month_scroll_settles_on_the_nearest_week() {
        let mut pos = MonthScroll::at_rest(date(2026, 11, 10));
        let top = pos.top;
        pos.scroll(0.6);
        while pos.settle(0.3) {}
        assert_eq!((pos.top, pos.offset), (top + Duration::days(7), 0.0));
        pos.scroll(-0.3);
        while pos.settle(0.3) {}
        assert_eq!((pos.top, pos.offset), (top + Duration::days(7), 0.0));
    }

    #[test]
    fn scrolled_month_rows_shift_and_clip_under_the_header() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, MONTH_HEADER_H + 6.0 * 90.0));
        let mut pos = MonthScroll::at_rest(day());
        pos.scroll(0.5);
        let lay = month(&[], pos, rect, &|_| 14.0, &|_| true);
        let first = &lay.cells[0];
        assert_eq!(first.day, pos.top);
        assert_eq!(first.rect.top(), MONTH_HEADER_H - 45.0, "half the top row is out of view");
        assert_eq!(lay.cells.len() as i64, 7 * (pos.rows + 1));
        // The hidden half of the top row isn't clickable through the header.
        let under_header = Pos2::new(first.rect.center().x, MONTH_HEADER_H - 10.0);
        assert!(lay.hits.hit(under_header).is_none());
        let below = Pos2::new(first.rect.center().x, MONTH_HEADER_H + 10.0);
        assert_eq!(lay.hits.hit(below), Some(&Target::MonthCell(first.day)));
    }
}
