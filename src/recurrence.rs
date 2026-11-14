//! Repeating events and time zones.
//!
//! A repeating event is stored once, as the *series*: its `start`/`end` are
//! the first occurrence and `recurrence` says how it repeats. The views never
//! show the series itself; [`occurrences`] expands it into one [`Event`] per
//! occurrence, each with an id of the form `<series id>#occ:<stamp>`.
//!
//! Times: `Event::start`/`end` are always wall-clock times on this computer
//! (what the views draw). An event with a `time_zone` is pinned to that zone:
//! the editor shows its times there, Google is told the zone, and a series is
//! repeated in that zone's wall time, so "every Monday 09:00 New York" stays
//! at 09:00 in New York across daylight-saving changes, wherever it's viewed.
//! An event without a zone is *floating*: it repeats in local wall time.
//!
//! "Series time" below means wall time in the event's zone (or local time for
//! a floating or all-day event). Occurrence stamps and exceptions are in
//! series time, so they don't move when the computer changes zone.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::dates::{add_months, day_short, first_of_month, midnight, week_index};
use crate::model::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    pub const ALL: [Freq; 4] = [Freq::Daily, Freq::Weekly, Freq::Monthly, Freq::Yearly];

    /// "day", "week", …, pluralized for `n`.
    pub fn unit(self, n: u32) -> String {
        let s = match self {
            Freq::Daily => "day",
            Freq::Weekly => "week",
            Freq::Monthly => "month",
            Freq::Yearly => "year",
        };
        if n == 1 {
            s.into()
        } else {
            format!("{s}s")
        }
    }

    fn rrule(self) -> &'static str {
        match self {
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepeatEnd {
    Never,
    /// The last day an occurrence may start on (in series time).
    Until(NaiveDate),
    /// The number of occurrences, counting removed ones (as RFC 5545 does).
    Count(u32),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recurrence {
    pub freq: Freq,
    /// Every `interval` days/weeks/…; at least 1.
    pub interval: u32,
    /// Weekly only: the days it repeats on. Empty: the first day's weekday.
    #[serde(default)]
    pub weekdays: Vec<Weekday>,
    pub end: RepeatEnd,
    /// Occurrences removed from the series, by their start in series time.
    #[serde(default)]
    pub exceptions: Vec<NaiveDateTime>,
}

impl Recurrence {
    pub fn new(freq: Freq) -> Self {
        Recurrence { freq, interval: 1, weekdays: Vec::new(), end: RepeatEnd::Never, exceptions: Vec::new() }
    }

    /// The rule without its exceptions, to tell whether the *rule* changed.
    pub fn rule_eq(&self, other: &Recurrence) -> bool {
        Recurrence { exceptions: Vec::new(), ..self.clone() } == Recurrence { exceptions: Vec::new(), ..other.clone() }
    }

    /// "Every week on Mon, Thu, until 31 Dec 2026".
    pub fn describe(&self, start: NaiveDate) -> String {
        let n = self.interval.max(1);
        let mut s = if n == 1 {
            format!("Every {}", self.freq.unit(1))
        } else {
            format!("Every {n} {}", self.freq.unit(n))
        };
        if self.freq == Freq::Weekly {
            let days = self.days_of_week(start);
            if days.len() > 1 || days[0] != start.weekday() {
                let names: Vec<&str> = days.iter().map(|&d| day_short(d)).collect();
                s += &format!(" on {}", names.join(", "));
            }
        }
        match self.end {
            RepeatEnd::Never => {}
            RepeatEnd::Until(d) => s += &format!(", until {}", d.format("%-d %b %Y")),
            RepeatEnd::Count(1) => s += ", once",
            RepeatEnd::Count(c) => s += &format!(", {c} times"),
        }
        s
    }

    /// The weekdays a weekly rule repeats on, in week order.
    fn days_of_week(&self, start: NaiveDate) -> Vec<Weekday> {
        let mut days = if self.weekdays.is_empty() { vec![start.weekday()] } else { self.weekdays.clone() };
        days.sort_by_key(|&d| week_index(d));
        days.dedup();
        days
    }

    /// Candidate starts (series time) for period `k`: the `k`-th day, week,
    /// month or year counted in steps of `interval` from `first`. Months and
    /// years without the start's day (the 31st, 29 February) are skipped, as
    /// in RFC 5545.
    fn period(&self, first: NaiveDateTime, k: i64, out: &mut Vec<NaiveDateTime>) {
        let step = k * self.interval.max(1) as i64;
        let time = first.time();
        match self.freq {
            Freq::Daily => out.push(first + Duration::days(step)),
            Freq::Weekly => {
                // Weeks start on Monday here, like RFC 5545's default WKST.
                let monday = first.date() - Duration::days(first.weekday().num_days_from_monday() as i64);
                let week = monday + Duration::weeks(step);
                let mut days = self.days_of_week(first.date());
                days.sort_by_key(|d| d.num_days_from_monday());
                for d in days {
                    let t = (week + Duration::days(d.num_days_from_monday() as i64)).and_time(time);
                    if t >= first {
                        out.push(t);
                    }
                }
            }
            Freq::Monthly => {
                let month = add_months(first_of_month(first.date()), step as i32);
                if let Some(d) = month.with_day(first.day()) {
                    out.push(d.and_time(time));
                }
            }
            Freq::Yearly => {
                let year = first.year() + step as i32;
                if let Some(d) = NaiveDate::from_ymd_opt(year, first.month(), first.day()) {
                    out.push(d.and_time(time));
                }
            }
        }
    }

    /// A period index at or before the one containing `t`, to skip straight
    /// to the visible range of a long-running series.
    fn period_before(&self, first: NaiveDateTime, t: NaiveDateTime) -> i64 {
        let days = (t.date() - first.date()).num_days();
        let n = self.interval.max(1) as i64;
        let periods = match self.freq {
            Freq::Daily => days / n,
            Freq::Weekly => days / 7 / n,
            Freq::Monthly => days / 31 / n,
            Freq::Yearly => days / 366 / n,
        };
        (periods - 1).max(0)
    }
}

// ---------------------------------------------------------------------------
// Time zones

pub fn parse_zone(name: &str) -> Option<Tz> {
    name.parse().ok()
}

/// This computer's zone, e.g. "Europe/Amsterdam".
pub fn local_zone_name() -> Option<String> {
    iana_time_zone::get_timezone().ok().filter(|n| parse_zone(n).is_some())
}

/// All zone names, for the picker.
pub fn zone_names() -> impl Iterator<Item = &'static str> {
    chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name())
}

/// Wall time `t` in zone `a`, as wall time in zone `b`. A time that doesn't
/// exist in `a` (skipped by a daylight-saving change) is moved forward an
/// hour first.
pub fn convert<A: TimeZone, B: TimeZone>(t: NaiveDateTime, a: &A, b: &B) -> NaiveDateTime {
    let at = |t| a.from_local_datetime(&t).earliest();
    match at(t).or_else(|| at(t + Duration::hours(1))) {
        Some(dt) => dt.with_timezone(b).naive_local(),
        None => t,
    }
}

/// The zone a timed event is pinned to (`None`: floating, or all-day).
pub fn zone_of(ev: &Event) -> Option<Tz> {
    if ev.all_day {
        return None;
    }
    ev.time_zone.as_deref().and_then(parse_zone)
}

/// Local wall time → wall time in `zone` (`None`: unchanged).
pub fn to_zone(t: NaiveDateTime, zone: Option<Tz>) -> NaiveDateTime {
    to_zone_from(t, zone, &Local)
}

/// Wall time in `zone` → local wall time (`None`: unchanged).
pub fn from_zone(t: NaiveDateTime, zone: Option<Tz>) -> NaiveDateTime {
    from_zone_to(t, zone, &Local)
}

fn to_zone_from<L: TimeZone>(t: NaiveDateTime, zone: Option<Tz>, local: &L) -> NaiveDateTime {
    zone.map_or(t, |z| convert(t, local, &z))
}

fn from_zone_to<L: TimeZone>(t: NaiveDateTime, zone: Option<Tz>, local: &L) -> NaiveDateTime {
    zone.map_or(t, |z| convert(t, &z, local))
}

// ---------------------------------------------------------------------------
// Occurrences

const OCC: &str = "#occ:";
const STAMP: &str = "%Y%m%dT%H%M%S";

/// The id shown for the occurrence of `series` that starts at `stamp`
/// (series time).
pub fn occurrence_id(series: &str, stamp: NaiveDateTime) -> String {
    format!("{series}{OCC}{}", stamp.format(STAMP))
}

/// Splits an occurrence id into the series id and the stamp.
pub fn split_occurrence(id: &str) -> Option<(&str, NaiveDateTime)> {
    let (series, stamp) = id.rsplit_once(OCC)?;
    Some((series, NaiveDateTime::parse_from_str(stamp, STAMP).ok()?))
}

/// The occurrences of a repeating event that overlap [`from`, `to`) (local
/// times). Each is a copy of the series with its own id and times.
pub fn occurrences(ev: &Event, from: NaiveDateTime, to: NaiveDateTime) -> Vec<Event> {
    occurrences_with(ev, from, to, &Local)
}

fn occurrences_with<L: TimeZone>(ev: &Event, from: NaiveDateTime, to: NaiveDateTime, local: &L) -> Vec<Event> {
    let Some(rule) = &ev.recurrence else { return Vec::new() };
    let zone = zone_of(ev);
    let first = to_zone_from(ev.start, zone, local);
    let len = to_zone_from(ev.end, zone, local) - first;
    // Counting needs every occurrence from the start; otherwise skip ahead.
    let counted = matches!(rule.end, RepeatEnd::Count(_));
    let earliest = to_zone_from(from, zone, local) - len.max(Duration::zero()) - Duration::days(1);
    let mut k = if counted { 0 } else { rule.period_before(first, earliest) };

    let mut out = Vec::new();
    let mut seen = 0u32;
    let mut candidates = Vec::new();
    // Guards against rules that never produce anything (or a series decades
    // long with a count); far more than any view shows.
    for _ in 0..20_000 {
        candidates.clear();
        rule.period(first, k, &mut candidates);
        k += 1;
        for &stamp in &candidates {
            if let RepeatEnd::Until(last) = rule.end {
                if stamp.date() > last {
                    return out;
                }
            }
            if let RepeatEnd::Count(n) = rule.end {
                if seen >= n {
                    return out;
                }
            }
            seen += 1;
            let start = from_zone_to(stamp, zone, local);
            if start >= to {
                return out;
            }
            if rule.exceptions.contains(&stamp) {
                continue;
            }
            let end = from_zone_to(stamp + len, zone, local);
            if end > from || (end <= start && start >= from) {
                let mut occ = ev.clone();
                occ.id = occurrence_id(&ev.id, stamp);
                occ.start = start;
                occ.end = end;
                out.push(occ);
            }
        }
    }
    out
}

/// The occurrence `id` names, if its series has one at that stamp.
pub fn occurrence(series: &Event, id: &str) -> Option<Event> {
    let (_, stamp) = split_occurrence(id)?;
    let start = from_zone(stamp, zone_of(series));
    let len = (series.end - series.start).max(Duration::zero());
    occurrences(series, start - Duration::hours(1), start + len + Duration::hours(1))
        .into_iter()
        .find(|o| o.id == id)
}

/// When an event (or occurrence) starts in its series time.
pub fn series_start(ev: &Event) -> NaiveDateTime {
    to_zone(ev.start, zone_of(ev))
}

/// Applies an edit of one occurrence (`edited`, which started at `stamp`) to
/// its whole series. The series moves by as much as the occurrence moved,
/// in series time, and its removed occurrences move along. With repeating
/// turned off, the event is left where the edited occurrence is.
///
/// Returns the updated series and the id the edited occurrence now has.
pub fn edit_series(series: &Event, stamp: NaiveDateTime, edited: &Event) -> (Event, String) {
    let mut s = edited.clone();
    s.id = series.id.clone();
    let Some(rule) = s.recurrence.as_mut() else {
        let shown = s.id.clone();
        return (s, shown);
    };
    // The edited occurrence's fields are in its (possibly new) zone, so
    // "09:00" stays 09:00 when only the zone changed.
    let zone = zone_of(edited);
    let mut new_stamp = to_zone(edited.start, zone);
    let delta = new_stamp - stamp;
    let mut first = series_start(series) + delta;
    if edited.all_day {
        first = midnight(first.date());
        new_stamp = midnight(new_stamp.date());
    }
    let len = to_zone(edited.end, zone) - to_zone(edited.start, zone);
    s.start = from_zone(first, zone);
    s.end = from_zone(first + len, zone);
    for x in &mut rule.exceptions {
        *x += delta;
    }
    let shown = occurrence_id(&s.id, new_stamp);
    (s, shown)
}

// ---------------------------------------------------------------------------
// RFC 5545 (what Google stores in an event's `recurrence`)

/// `RRULE` and `EXDATE` lines for Google. `zone` names the zone that timed
/// stamps are in.
pub fn to_rrule(rule: &Recurrence, ev: &Event, zone: &str) -> Vec<String> {
    let mut r = format!("RRULE:FREQ={}", rule.freq.rrule());
    if rule.interval > 1 {
        r += &format!(";INTERVAL={}", rule.interval);
    }
    if rule.freq == Freq::Weekly && !rule.weekdays.is_empty() {
        let days: Vec<&str> = rule.days_of_week(ev.start.date()).iter().map(|&d| &day_short(d)[..2]).collect();
        r += &format!(";BYDAY={}", days.join(",").to_uppercase());
    }
    match rule.end {
        RepeatEnd::Never => {}
        RepeatEnd::Count(n) => r += &format!(";COUNT={n}"),
        RepeatEnd::Until(d) if ev.all_day => r += &format!(";UNTIL={}", d.format("%Y%m%d")),
        RepeatEnd::Until(d) => {
            // The end of that day in the event's zone, in UTC (RFC 5545 wants
            // UTC when the start has a zone).
            let last = d.and_hms_opt(23, 59, 59).unwrap();
            let utc = match parse_zone(zone) {
                Some(tz) => convert(last, &tz, &Utc),
                None => last,
            };
            r += &format!(";UNTIL={}Z", utc.format(STAMP));
        }
    }
    let mut lines = vec![r];
    if !rule.exceptions.is_empty() {
        let mut ex = rule.exceptions.clone();
        ex.sort();
        lines.push(if ev.all_day {
            let days: Vec<String> = ex.iter().map(|t| t.format("%Y%m%d").to_string()).collect();
            format!("EXDATE;VALUE=DATE:{}", days.join(","))
        } else {
            let times: Vec<String> = ex.iter().map(|t| t.format(STAMP).to_string()).collect();
            format!("EXDATE;TZID={zone}:{}", times.join(","))
        });
    }
    lines
}

/// Reads Google's `recurrence` lines back. Rules this app can't represent
/// (e.g. "the second Tuesday of every month") give `None`.
pub fn from_rrule(lines: &[String], all_day: bool, zone: Option<Tz>) -> Option<Recurrence> {
    let mut rule: Option<Recurrence> = None;
    let mut exceptions = Vec::new();
    for line in lines {
        let (head, value) = line.split_once(':')?;
        let name = head.split(';').next().unwrap_or(head);
        match name {
            "RRULE" => {
                let mut r = Recurrence::new(Freq::Daily);
                for part in value.split(';') {
                    let (k, v) = part.split_once('=')?;
                    match k {
                        "FREQ" => {
                            r.freq = match v {
                                "DAILY" => Freq::Daily,
                                "WEEKLY" => Freq::Weekly,
                                "MONTHLY" => Freq::Monthly,
                                "YEARLY" => Freq::Yearly,
                                _ => return None,
                            }
                        }
                        "INTERVAL" => r.interval = v.parse().ok().filter(|&n| n >= 1)?,
                        "COUNT" => r.end = RepeatEnd::Count(v.parse().ok()?),
                        "UNTIL" => r.end = RepeatEnd::Until(parse_until(v, zone)?),
                        "BYDAY" => {
                            r.weekdays = v.split(',').map(parse_weekday).collect::<Option<_>>()?;
                        }
                        "WKST" => {}
                        _ => return None,
                    }
                }
                if !r.weekdays.is_empty() && r.freq != Freq::Weekly {
                    return None;
                }
                rule = Some(r);
            }
            "EXDATE" => {
                let tzid = head.split(';').find_map(|p| p.strip_prefix("TZID=")).and_then(parse_zone);
                for v in value.split(',') {
                    exceptions.push(parse_exdate(v, all_day, tzid, zone)?);
                }
            }
            // RDATE and EXRULE: more than this app models.
            _ => return None,
        }
    }
    let mut rule = rule?;
    rule.exceptions = exceptions;
    Some(rule)
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// An RFC 5545 date or date-time: "20261231", "20261231T235959" or
/// "20261231T235959Z" (UTC). Returns the date-time and whether it was UTC.
fn parse_stamp(v: &str) -> Option<(NaiveDateTime, bool)> {
    if let Some(utc) = v.strip_suffix('Z') {
        return Some((NaiveDateTime::parse_from_str(utc, STAMP).ok()?, true));
    }
    if v.len() == 8 {
        return Some((midnight(NaiveDate::parse_from_str(v, "%Y%m%d").ok()?), false));
    }
    Some((NaiveDateTime::parse_from_str(v, STAMP).ok()?, false))
}

fn parse_until(v: &str, zone: Option<Tz>) -> Option<NaiveDate> {
    let (t, utc) = parse_stamp(v)?;
    Some(match (utc, zone) {
        (true, Some(z)) => convert(t, &Utc, &z).date(),
        (true, None) => convert(t, &Utc, &Local).date(),
        (false, _) => t.date(),
    })
}

fn parse_exdate(v: &str, all_day: bool, tzid: Option<Tz>, zone: Option<Tz>) -> Option<NaiveDateTime> {
    let (t, utc) = parse_stamp(v)?;
    if all_day {
        return Some(midnight(t.date()));
    }
    let instant: DateTime<Utc> = match tzid {
        _ if utc => t.and_utc(),
        Some(tz) if Some(tz) != zone => tz.from_local_datetime(&t).earliest()?.with_timezone(&Utc),
        // Already in the event's own zone.
        _ => return Some(t),
    };
    // Into series time: the event's zone, or local for a floating event.
    Some(match zone {
        Some(z) => instant.with_timezone(&z).naive_local(),
        None => instant.with_timezone(&Local).naive_local(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn at(day: NaiveDate, h: u32, m: u32) -> NaiveDateTime {
        day.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap())
    }

    fn series(start: NaiveDateTime, minutes: i64, rule: Recurrence) -> Event {
        Event {
            id: "local:s".into(),
            calendar_id: "local:home".into(),
            title: "Standup".into(),
            start,
            end: start + Duration::minutes(minutes),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
            recurrence: Some(rule),
            time_zone: None,
        }
    }

    fn starts(ev: &Event, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDateTime> {
        occurrences_with(ev, midnight(from), midnight(to), &Utc).iter().map(|o| o.start).collect()
    }

    #[test]
    fn daily_with_interval_count_and_exceptions() {
        let mut rule = Recurrence::new(Freq::Daily);
        rule.interval = 2;
        rule.end = RepeatEnd::Count(4);
        rule.exceptions = vec![at(d(2026, 11, 5), 9, 0)];
        let ev = series(at(d(2026, 11, 1), 9, 0), 30, rule);
        // 1, 3, (5 removed but counted), 7; then the count is used up.
        assert_eq!(
            starts(&ev, d(2026, 10, 1), d(2027, 1, 1)),
            [at(d(2026, 11, 1), 9, 0), at(d(2026, 11, 3), 9, 0), at(d(2026, 11, 7), 9, 0)]
        );
        let occ = &occurrences_with(&ev, midnight(d(2026, 11, 3)), midnight(d(2026, 11, 4)), &Utc)[0];
        assert_eq!(occ.id, "local:s#occ:20261103T090000");
        assert_eq!(occ.end - occ.start, Duration::minutes(30));
        assert_eq!(split_occurrence(&occ.id), Some(("local:s", at(d(2026, 11, 3), 9, 0))));
    }

    #[test]
    fn weekly_on_several_days_until_a_date() {
        let mut rule = Recurrence::new(Freq::Weekly);
        rule.weekdays = vec![Weekday::Thu, Weekday::Mon];
        rule.end = RepeatEnd::Until(d(2026, 11, 19));
        // Starts on a Wednesday: the first week only has Thursday left.
        let ev = series(at(d(2026, 11, 11), 10, 0), 60, rule.clone());
        assert_eq!(
            starts(&ev, d(2026, 11, 1), d(2026, 12, 31)),
            [at(d(2026, 11, 12), 10, 0), at(d(2026, 11, 16), 10, 0), at(d(2026, 11, 19), 10, 0)]
        );
        assert_eq!(rule.describe(d(2026, 11, 11)), "Every week on Mon, Thu, until 19 Nov 2026");
    }

    #[test]
    fn monthly_and_yearly_skip_missing_days() {
        let ev = series(at(d(2026, 1, 31), 8, 0), 15, Recurrence::new(Freq::Monthly));
        let got: Vec<NaiveDate> = starts(&ev, d(2026, 1, 1), d(2026, 6, 1)).iter().map(|t| t.date()).collect();
        assert_eq!(got, [d(2026, 1, 31), d(2026, 3, 31), d(2026, 5, 31)]);

        let ev = series(at(d(2024, 2, 29), 8, 0), 15, Recurrence::new(Freq::Yearly));
        let got: Vec<NaiveDate> = starts(&ev, d(2024, 1, 1), d(2033, 1, 1)).iter().map(|t| t.date()).collect();
        assert_eq!(got, [d(2024, 2, 29), d(2028, 2, 29), d(2032, 2, 29)]);
    }

    #[test]
    fn a_long_running_series_only_expands_the_visible_range() {
        let ev = series(at(d(2000, 1, 1), 9, 0), 60, Recurrence::new(Freq::Daily));
        assert_eq!(starts(&ev, d(2026, 11, 14), d(2026, 11, 16)), [at(d(2026, 11, 14), 9, 0), at(d(2026, 11, 15), 9, 0)]);
        // A multi-day occurrence that started before the range still shows.
        let mut long = series(at(d(2026, 11, 1), 9, 0), 3 * 24 * 60, Recurrence::new(Freq::Weekly));
        long.recurrence.as_mut().unwrap().end = RepeatEnd::Never;
        assert_eq!(starts(&long, d(2026, 11, 10), d(2026, 11, 11)), [at(d(2026, 11, 8), 9, 0)]);
    }

    #[test]
    fn a_zoned_series_keeps_its_wall_time_across_daylight_saving() {
        // 09:00 every Monday in New York, seen from Amsterdam. In 2026 Europe
        // leaves daylight saving on 25 October and the US on 1 November, so
        // for one week the gap is 5 hours instead of 6.
        let ams: Tz = "Europe/Amsterdam".parse().unwrap();
        let ny: Tz = "America/New_York".parse().unwrap();
        let mut ev = series(convert(at(d(2026, 10, 19), 9, 0), &ny, &ams), 60, Recurrence::new(Freq::Weekly));
        ev.time_zone = Some("America/New_York".into());
        let got: Vec<NaiveDateTime> =
            occurrences_with(&ev, midnight(d(2026, 10, 19)), midnight(d(2026, 11, 3)), &ams).iter().map(|o| o.start).collect();
        assert_eq!(got, [at(d(2026, 10, 19), 15, 0), at(d(2026, 10, 26), 14, 0), at(d(2026, 11, 2), 15, 0)]);
        // A floating series stays at the same local time instead.
        ev.time_zone = None;
        let got: Vec<u32> = occurrences_with(&ev, midnight(d(2026, 10, 19)), midnight(d(2026, 11, 3)), &ams)
            .iter()
            .map(|o| chrono::Timelike::hour(&o.start))
            .collect();
        assert_eq!(got, [15, 15, 15]);
    }

    #[test]
    fn zone_conversion_handles_the_skipped_hour() {
        let ams: Tz = "Europe/Amsterdam".parse().unwrap();
        // 02:30 on 29 March 2026 doesn't exist in Amsterdam.
        assert_eq!(convert(at(d(2026, 3, 29), 2, 30), &ams, &Utc), at(d(2026, 3, 29), 1, 30));
        assert_eq!(convert(at(d(2026, 7, 1), 12, 0), &ams, &Utc), at(d(2026, 7, 1), 10, 0));
    }

    #[test]
    fn editing_one_occurrence_for_all_moves_the_whole_series() {
        let mut rule = Recurrence::new(Freq::Daily);
        rule.exceptions = vec![at(d(2026, 11, 3), 9, 0)];
        let s = series(at(d(2026, 11, 1), 9, 0), 30, rule);
        // The 5 November occurrence moved to 10:15 and made an hour long.
        let mut edited = s.clone();
        edited.id = occurrence_id(&s.id, at(d(2026, 11, 5), 9, 0));
        edited.title = "Later standup".into();
        edited.start = at(d(2026, 11, 5), 10, 15);
        edited.end = at(d(2026, 11, 5), 11, 15);
        let (moved, shown) = edit_series(&s, at(d(2026, 11, 5), 9, 0), &edited);
        assert_eq!((moved.id.as_str(), moved.title.as_str()), ("local:s", "Later standup"));
        assert_eq!((moved.start, moved.end), (at(d(2026, 11, 1), 10, 15), at(d(2026, 11, 1), 11, 15)));
        assert_eq!(moved.recurrence.as_ref().unwrap().exceptions, [at(d(2026, 11, 3), 10, 15)], "removed one stays removed");
        assert_eq!(shown, "local:s#occ:20261105T101500");

        // Made all-day: the series becomes all-day from its first day.
        let mut all_day = edited.clone();
        all_day.all_day = true;
        all_day.start = midnight(d(2026, 11, 5));
        all_day.end = midnight(d(2026, 11, 6));
        let (moved, shown) = edit_series(&s, at(d(2026, 11, 5), 9, 0), &all_day);
        assert_eq!((moved.start, moved.end), (midnight(d(2026, 11, 1)), midnight(d(2026, 11, 2))));
        assert_eq!(shown, "local:s#occ:20261105T000000");

        // Repeat turned off: what's left is the occurrence that was edited.
        let mut single = edited.clone();
        single.recurrence = None;
        let (moved, shown) = edit_series(&s, at(d(2026, 11, 5), 9, 0), &single);
        assert_eq!((moved.start, moved.recurrence, shown.as_str()), (at(d(2026, 11, 5), 10, 15), None, "local:s"));
    }

    #[test]
    fn changing_a_series_zone_keeps_its_wall_time() {
        // A floating 09:00 series pinned to New York stays at 09:00, now
        // New York time (the editor keeps the fields as they were).
        let s = series(at(d(2026, 11, 2), 9, 0), 60, Recurrence::new(Freq::Weekly));
        let ny = parse_zone("America/New_York");
        let mut edited = s.clone();
        edited.time_zone = Some("America/New_York".into());
        edited.start = from_zone(at(d(2026, 11, 9), 9, 0), ny);
        edited.end = from_zone(at(d(2026, 11, 9), 10, 0), ny);
        let (moved, shown) = edit_series(&s, at(d(2026, 11, 9), 9, 0), &edited);
        assert_eq!(shown, "local:s#occ:20261109T090000");
        assert_eq!(series_start(&moved), at(d(2026, 11, 2), 9, 0));
        assert_eq!(to_zone(moved.end, ny) - to_zone(moved.start, ny), Duration::hours(1));
    }

    #[test]
    fn rrule_round_trip() {
        let mut rule = Recurrence::new(Freq::Weekly);
        rule.interval = 2;
        rule.weekdays = vec![Weekday::Mon, Weekday::Wed];
        rule.end = RepeatEnd::Until(d(2026, 12, 31));
        rule.exceptions = vec![at(d(2026, 11, 18), 9, 0)];
        let mut ev = series(at(d(2026, 11, 16), 9, 0), 60, rule.clone());
        ev.time_zone = Some("Europe/Amsterdam".into());
        let lines = to_rrule(&rule, &ev, "Europe/Amsterdam");
        assert_eq!(
            lines,
            [
                "RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;UNTIL=20261231T225959Z",
                "EXDATE;TZID=Europe/Amsterdam:20261118T090000",
            ]
        );
        let ams = parse_zone("Europe/Amsterdam");
        assert_eq!(from_rrule(&lines, false, ams), Some(rule));

        let mut all_day = Recurrence::new(Freq::Yearly);
        all_day.end = RepeatEnd::Count(3);
        all_day.exceptions = vec![midnight(d(2027, 5, 1))];
        let mut ev = series(midnight(d(2026, 5, 1)), 24 * 60, all_day.clone());
        ev.all_day = true;
        let lines = to_rrule(&all_day, &ev, "UTC");
        assert_eq!(lines, ["RRULE:FREQ=YEARLY;COUNT=3", "EXDATE;VALUE=DATE:20270501"]);
        assert_eq!(from_rrule(&lines, true, None), Some(all_day));

        // Rules beyond what the app models aren't half-understood.
        assert_eq!(from_rrule(&["RRULE:FREQ=MONTHLY;BYDAY=2TU".into()], false, None), None);
        assert_eq!(from_rrule(&["RRULE:FREQ=MONTHLY;BYSETPOS=-1".into()], false, None), None);
    }
}
