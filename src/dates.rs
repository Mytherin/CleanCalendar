//! Small date/time helpers.

use chrono::{Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};

/// Weeks start on Monday (as in most of Europe); `false` starts on Sunday.
pub const WEEK_STARTS_MONDAY: bool = true;
/// 24-hour clock ("14:30"); `false` uses "2:30 PM".
pub const CLOCK_24H: bool = true;

/// Position of a weekday within the week (0 = first column).
pub fn week_index(wd: Weekday) -> u32 {
    if WEEK_STARTS_MONDAY {
        wd.num_days_from_monday()
    } else {
        wd.num_days_from_sunday()
    }
}

pub fn start_of_week(d: NaiveDate) -> NaiveDate {
    d - Duration::days(week_index(d.weekday()) as i64)
}

pub fn is_weekend(d: NaiveDate) -> bool {
    matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

/// The weekday shown in week column `i`.
pub fn column_weekday(i: usize) -> Weekday {
    let first = if WEEK_STARTS_MONDAY { Weekday::Mon } else { Weekday::Sun };
    (0..i % 7).fold(first, |wd, _| wd.succ())
}

/// Whether week column `i` is Saturday or Sunday.
pub fn is_weekend_column(i: usize) -> bool {
    matches!(column_weekday(i), Weekday::Sat | Weekday::Sun)
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

/// "14:30" (or "2:30 PM" on a 12-hour clock)
pub fn fmt_time(t: NaiveTime) -> String {
    if CLOCK_24H {
        return format!("{:02}:{:02}", t.hour(), t.minute());
    }
    let (pm, h) = t.hour12();
    let ampm = if pm { "PM" } else { "AM" };
    format!("{h}:{:02} {ampm}", t.minute())
}

/// "11:45–12:30", the way event blocks show their time.
pub fn fmt_time_range(start: NaiveTime, end: NaiveTime) -> String {
    format!("{}\u{2013}{}", fmt_time(start), fmt_time(end))
}

/// Hour label in the time grid: "07:00" (or "7 AM", "Noon").
pub fn fmt_hour(h: u32) -> String {
    if CLOCK_24H {
        return format!("{h:02}:00");
    }
    match h {
        0 => "12 AM".into(),
        12 => "Noon".into(),
        h if h < 12 => format!("{h} AM"),
        h => format!("{} PM", h - 12),
    }
}

/// Short weekday name for week column `i` ("Mon", …).
pub fn weekday_short(i: usize) -> &'static str {
    day_short(column_weekday(i))
}

/// One-letter weekday for week column `i` ("M", …).
pub fn weekday_initial(i: usize) -> &'static str {
    &day_short(column_weekday(i))[..1]
}

pub fn day_short(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn week_starts_on_monday() {
        assert_eq!(start_of_week(d(2026, 11, 14)), d(2026, 11, 9));
        assert_eq!(start_of_week(d(2026, 11, 9)), d(2026, 11, 9));
        assert_eq!(start_of_week(d(2026, 11, 15)), d(2026, 11, 9), "Sunday ends the week");
        assert_eq!((weekday_short(0), weekday_initial(6)), ("Mon", "S"));
        assert!(is_weekend_column(5) && is_weekend_column(6) && !is_weekend_column(4));
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
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!(fmt_time(t(0, 5)), "00:05");
        assert_eq!(fmt_time_range(t(11, 45), t(12, 30)), "11:45\u{2013}12:30");
        assert_eq!((fmt_hour(7), fmt_hour(13)), ("07:00".into(), "13:00".into()));
    }

    #[test]
    fn all_day_end_is_exclusive() {
        let s = midnight(d(2026, 11, 11));
        let e = midnight(d(2026, 11, 14));
        assert_eq!(last_day(s, e), d(2026, 11, 13));
    }
}
