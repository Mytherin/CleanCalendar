//! Small date/time helpers.

use chrono::{Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

pub fn start_of_week(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_sunday() as i64)
}

pub fn first_of_month(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap()
}

pub fn add_months(d: NaiveDate, n: i32) -> NaiveDate {
    let r = if n >= 0 {
        d.checked_add_months(Months::new(n as u32))
    } else {
        d.checked_sub_months(Months::new(n.unsigned_abs()))
    };
    r.unwrap_or(d)
}

pub fn days_in_month(d: NaiveDate) -> u32 {
    let first = first_of_month(d);
    (add_months(first, 1) - first).num_days() as u32
}

pub fn midnight(d: NaiveDate) -> NaiveDateTime {
    d.and_hms_opt(0, 0, 0).unwrap()
}

/// Minutes since midnight.
pub fn minute_of_day(t: NaiveDateTime) -> i64 {
    (t.hour() * 60 + t.minute()) as i64
}

/// Round to the nearest quarter hour.
pub fn snap(t: NaiveDateTime) -> NaiveDateTime {
    let m = minute_of_day(t);
    midnight(t.date()) + Duration::minutes((m + 7) / 15 * 15)
}

/// The last calendar day an event touches (its end is exclusive).
pub fn last_day(start: NaiveDateTime, end: NaiveDateTime) -> NaiveDate {
    if end <= start {
        start.date()
    } else {
        (end - Duration::seconds(1)).date()
    }
}

/// "9:30 AM"
pub fn fmt_time(t: NaiveTime) -> String {
    let (pm, h) = t.hour12();
    let ampm = if pm { "PM" } else { "AM" };
    format!("{h}:{:02} {ampm}", t.minute())
}

/// "9 AM", "Noon"
pub fn fmt_hour(h: u32) -> String {
    match h {
        0 => "12 AM".into(),
        12 => "Noon".into(),
        h if h < 12 => format!("{h} AM"),
        h => format!("{} PM", h - 12),
    }
}

pub fn weekday_short(i: usize) -> &'static str {
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][i % 7]
}

pub fn weekday_initial(i: usize) -> &'static str {
    ["S", "M", "T", "W", "T", "F", "S"][i % 7]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn week_starts_on_sunday() {
        assert_eq!(start_of_week(d(2026, 11, 14)), d(2026, 11, 8));
        assert_eq!(start_of_week(d(2026, 11, 8)), d(2026, 11, 8));
    }

    #[test]
    fn month_math() {
        assert_eq!(add_months(d(2026, 1, 31), 1), d(2026, 2, 28));
        assert_eq!(add_months(d(2026, 1, 15), -1), d(2025, 12, 15));
        assert_eq!(days_in_month(d(2028, 2, 10)), 29);
    }

    #[test]
    fn snapping_and_formatting() {
        let t = d(2026, 1, 1).and_hms_opt(9, 8, 0).unwrap();
        assert_eq!(snap(t), d(2026, 1, 1).and_hms_opt(9, 15, 0).unwrap());
        assert_eq!(fmt_time(NaiveTime::from_hms_opt(0, 5, 0).unwrap()), "12:05 AM");
        assert_eq!(fmt_hour(13), "1 PM");
    }

    #[test]
    fn all_day_end_is_exclusive() {
        let s = midnight(d(2026, 11, 11));
        let e = midnight(d(2026, 11, 14));
        assert_eq!(last_day(s, e), d(2026, 11, 13));
    }
}
