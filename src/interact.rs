//! Pointer interaction for the event views, as a pure state machine.
//!
//! The views turn egui's raw pointer events into [`Input`]s and feed them to
//! [`Pointer::handle`] together with the current [`HitMap`]. It works out
//! clicks, double-clicks and drags itself and answers with [`Action`]s for the
//! app to apply. Because it never sees egui, it can be driven by unit tests
//! (see `tests` below) with synthetic input against real layouts.

use chrono::{Duration, NaiveDate, NaiveDateTime};
use eframe::egui::Pos2;

use crate::app::{Action, View};
use crate::dates::*;
use crate::layout::{Grip, HitMap, Target, TimeGrid};

/// How far the pointer must move before a press becomes a drag.
const DRAG_DIST: f32 = 6.0;
/// Max seconds between the clicks of a double-click.
const DOUBLE_CLICK_S: f64 = 0.3;

#[derive(Clone, Copy, Debug)]
pub enum Input {
    Press { pos: Pos2 },
    Move { pos: Pos2 },
    Release { pos: Pos2, time: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Grabbing,
    ResizeVertical,
    PointingHand,
}

#[derive(Clone, Debug)]
enum Drag {
    Create { anchor: NaiveDateTime, current: NaiveDateTime },
    Move { id: String, grab: Duration, len: Duration },
    Resize { id: String, start: NaiveDateTime },
    MoveDays { id: String, grab: i64, start: NaiveDateTime, end: NaiveDateTime },
}

#[derive(Clone, Debug, Default)]
enum Gesture {
    #[default]
    Idle,
    /// Button down, not yet moved far enough to be a drag.
    Pressed { pos: Pos2, target: Target },
    Dragging(Drag),
    /// A drag on something that can't be dragged: swallow it until release.
    Ignored,
}

#[derive(Clone, Debug)]
struct Click {
    time: f64,
    pos: Pos2,
    target: Target,
}

#[derive(Default)]
pub struct Pointer {
    gesture: Gesture,
    last_click: Option<Click>,
}

impl Pointer {
    /// Whether a press is in progress (so a lost release must be synthesized).
    pub fn is_active(&self) -> bool {
        !matches!(self.gesture, Gesture::Idle)
    }

    /// The event being moved or resized, if any.
    pub fn dragging_id(&self) -> Option<&str> {
        match &self.gesture {
            Gesture::Dragging(Drag::Move { id, .. } | Drag::Resize { id, .. } | Drag::MoveDays { id, .. }) => Some(id),
            _ => None,
        }
    }

    /// The event a drag refers to got a new id (e.g. after syncing).
    pub fn rename(&mut self, from: &str, to: &str) {
        if let Gesture::Dragging(Drag::Move { id, .. } | Drag::Resize { id, .. } | Drag::MoveDays { id, .. }) = &mut self.gesture {
            if id == from {
                *id = to.to_owned();
            }
        }
        if let Some(Click { target: Target::Event { id, .. }, .. }) = &mut self.last_click {
            if id == from {
                *id = to.to_owned();
            }
        }
    }

    /// The new event being drawn by drag-to-create, as (start, end).
    pub fn ghost(&self) -> Option<(NaiveDateTime, NaiveDateTime)> {
        match &self.gesture {
            Gesture::Dragging(Drag::Create { anchor, current }) => {
                let (s, e) = if current < anchor { (*current, *anchor) } else { (*anchor, *current) };
                Some((s, e.max(s + Duration::minutes(15))))
            }
            _ => None,
        }
    }

    /// The cursor to show, given what the pointer hovers.
    pub fn cursor(&self, hover: Option<&Target>) -> Option<Cursor> {
        match &self.gesture {
            Gesture::Dragging(Drag::Move { .. } | Drag::MoveDays { .. }) => Some(Cursor::Grabbing),
            Gesture::Dragging(Drag::Resize { .. }) => Some(Cursor::ResizeVertical),
            Gesture::Idle => match hover {
                Some(Target::Event { grip: Grip::Resize, .. }) => Some(Cursor::ResizeVertical),
                Some(Target::More(_)) => Some(Cursor::PointingHand),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn handle(&mut self, input: Input, hits: &HitMap) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::Press { pos } => {
                self.gesture = match hits.hit(pos) {
                    Some(target) => Gesture::Pressed { pos, target: target.clone() },
                    None => Gesture::Idle,
                };
            }
            Input::Move { pos } => {
                if let Gesture::Pressed { pos: origin, target } = &self.gesture {
                    if origin.distance(pos) >= DRAG_DIST {
                        self.gesture = match start_drag(target, *origin, hits.grid, &mut out) {
                            Some(drag) => Gesture::Dragging(drag),
                            None => Gesture::Ignored,
                        };
                        self.last_click = None;
                    }
                }
                if let (Gesture::Dragging(drag), Some(grid)) = (&mut self.gesture, hits.grid) {
                    update_drag(drag, pos, &grid, &mut out);
                }
            }
            Input::Release { pos, time } => match std::mem::take(&mut self.gesture) {
                Gesture::Pressed { target, .. } => {
                    let double = self.last_click.take().is_some_and(|c| {
                        time - c.time <= DOUBLE_CLICK_S && c.pos.distance(pos) < DRAG_DIST && same_target(&c.target, &target)
                    });
                    if double {
                        double_click(&target, pos, hits.grid, &mut out);
                    } else {
                        click(&target, &mut out);
                        self.last_click = Some(Click { time, pos, target });
                    }
                }
                Gesture::Dragging(drag) => finish_drag(drag, &mut out),
                Gesture::Idle | Gesture::Ignored => {}
            },
        }
        out
    }
}

/// Click targets match across the clicks of a double-click even if an
/// event's times changed in between.
fn same_target(a: &Target, b: &Target) -> bool {
    match (a, b) {
        (Target::Event { id: x, .. }, Target::Event { id: y, .. }) => x == y,
        _ => a == b,
    }
}

fn click(target: &Target, out: &mut Vec<Action>) {
    match target {
        Target::Event { id, .. } => out.push(Action::Select(Some(id.clone()))),
        Target::TimeGrid | Target::AllDayLane | Target::MonthCell(_) => out.push(Action::Select(None)),
        Target::MonthDayNumber(day) => {
            out.push(Action::Select(None));
            out.push(Action::GoTo(*day, View::Day));
        }
        Target::More(day) => out.push(Action::GoTo(*day, View::Day)),
        Target::DayHeader(_) => {}
    }
}

fn double_click(target: &Target, pos: Pos2, grid: Option<TimeGrid>, out: &mut Vec<Action>) {
    match target {
        Target::Event { id, .. } => out.push(Action::Open(id.clone())),
        Target::TimeGrid => {
            if let Some(g) = grid {
                let t = g.time_at(pos);
                let start = midnight(t.date()) + Duration::minutes(minute_of_day(t) / 30 * 30);
                out.push(Action::New { start, end: start + Duration::hours(1), all_day: false });
            }
        }
        Target::AllDayLane => {
            if let Some(g) = grid {
                let day = midnight(g.day_at(pos.x));
                out.push(Action::New { start: day, end: day + Duration::days(1), all_day: true });
            }
        }
        Target::MonthCell(day) | Target::MonthDayNumber(day) => {
            let start = midnight(*day) + Duration::hours(9);
            out.push(Action::New { start, end: start + Duration::hours(1), all_day: false });
        }
        Target::DayHeader(day) => out.push(Action::GoTo(*day, View::Day)),
        Target::More(day) => out.push(Action::GoTo(*day, View::Day)),
    }
}

fn start_drag(target: &Target, origin: Pos2, grid: Option<TimeGrid>, out: &mut Vec<Action>) -> Option<Drag> {
    let grid = grid?;
    let drag = match target {
        Target::TimeGrid => {
            let t = snap(grid.time_at(origin));
            return Some(Drag::Create { anchor: t, current: t });
        }
        Target::Event { id, start, end, grip } => match grip {
            Grip::Move => Drag::Move { id: id.clone(), grab: grid.time_at(origin) - *start, len: *end - *start },
            Grip::Resize => Drag::Resize { id: id.clone(), start: *start },
            Grip::MoveDays => Drag::MoveDays { id: id.clone(), grab: day_offset(grid.day_at(origin.x), start.date()), start: *start, end: *end },
            Grip::None => return None,
        },
        _ => return None,
    };
    let id = match &drag {
        Drag::Move { id, .. } | Drag::Resize { id, .. } | Drag::MoveDays { id, .. } => id.clone(),
        Drag::Create { .. } => unreachable!(),
    };
    out.push(Action::Select(Some(id.clone())));
    out.push(Action::BeginEdit(id));
    Some(drag)
}

fn day_offset(a: NaiveDate, b: NaiveDate) -> i64 {
    (a - b).num_days()
}

fn update_drag(drag: &mut Drag, pos: Pos2, grid: &TimeGrid, out: &mut Vec<Action>) {
    let set = |id: &str, start, end| Action::SetTimes { id: id.to_owned(), start, end };
    match drag {
        Drag::Create { anchor, current } => {
            let t = snap(grid.time_at(pos));
            // Drag-to-create stays within the day it started on.
            *current = if t.date() == anchor.date() { t } else { midnight(anchor.date()) + Duration::minutes(minute_of_day(t)) };
        }
        Drag::Move { id, grab, len } => {
            let start = snap(grid.time_at(pos) - *grab);
            out.push(set(id, start, start + *len));
        }
        Drag::Resize { id, start } => {
            let end = snap(grid.time_at(pos)).max(*start + Duration::minutes(15));
            out.push(set(id, *start, end));
        }
        Drag::MoveDays { id, grab, start, end } => {
            let delta = grid.day_at(pos.x) - Duration::days(*grab) - start.date();
            out.push(set(id, *start + delta, *end + delta));
        }
    }
}

fn finish_drag(drag: Drag, out: &mut Vec<Action>) {
    match drag {
        Drag::Create { anchor, current } => {
            let (mut s, mut e) = if current < anchor { (current, anchor) } else { (anchor, current) };
            if e - s < Duration::minutes(15) {
                s = anchor;
                e = anchor + Duration::hours(1);
            }
            out.push(Action::New { start: s, end: e, all_day: false });
        }
        Drag::Move { id, .. } | Drag::Resize { id, .. } | Drag::MoveDays { id, .. } => out.push(Action::Committed(id)),
    }
}

#[cfg(test)]
mod tests;
