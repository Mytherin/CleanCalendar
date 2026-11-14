//! User preferences: what the Settings window edits. Saved in `data.db`
//! (see [`crate::db`]) as one JSON value, so adding a setting needs no
//! schema change; missing fields take their defaults.

use std::sync::OnceLock;

use chrono::Weekday;
use serde::{Deserialize, Serialize};

use crate::dates;

/// How the week view's ‹ › buttons (and arrow keys) move.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeekScroll {
    /// A whole week at a time; weeks always start on the first day of the week.
    #[default]
    Week,
    /// One day at a time, so any 7 consecutive days can be shown.
    Day,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ---- General
    /// Days in the week view: 7, or 5 (Monday to Friday).
    pub days_per_week: u8,
    /// First day of the week, as days from Monday (0 = Monday, 6 = Sunday).
    /// `None` follows the system's region settings.
    pub week_start: Option<u8>,
    pub week_scroll: WeekScroll,
    /// Working hours, as whole hours (`day_end` may be 24). Hours outside them
    /// are shaded, and the day/week views open scrolled to `day_start`.
    pub day_start: u8,
    pub day_end: u8,
    /// How many hours fill the visible part of the day/week grid.
    pub hours_shown: u8,
    /// Length of a new event made by double-clicking or Ctrl+N.
    pub event_minutes: u32,
    /// The calendar new events go to; `None` uses the selected event's
    /// calendar (or the first one shown).
    pub default_calendar: Option<String>,
    pub show_birthdays: bool,
    pub show_holidays: bool,

    // ---- Accounts
    /// When off, the Google account's calendars are hidden and it isn't synced.
    pub google_enabled: bool,
    /// The account's name in the calendar list; empty shows its email.
    pub google_description: String,
    /// How often to pull changes from Google; 0 = only when asked.
    pub refresh_minutes: u32,

    // ---- Advanced
    /// Shade the year view's days by how many events they have.
    pub year_heatmap: bool,
    pub week_numbers: bool,
    /// How many months of past events synced calendars keep; 0 = all.
    pub history_months: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            days_per_week: 7,
            week_start: None,
            week_scroll: WeekScroll::Week,
            day_start: 8,
            day_end: 18,
            hours_shown: 12,
            event_minutes: 60,
            default_calendar: None,
            show_birthdays: true,
            show_holidays: true,
            google_enabled: true,
            google_description: String::new(),
            refresh_minutes: 5,
            year_heatmap: false,
            week_numbers: false,
            history_months: 0,
        }
    }
}

/// Choices offered for "Refresh Calendars", in minutes (0 = manually).
pub const REFRESH_CHOICES: [u32; 6] = [1, 5, 15, 30, 60, 0];
/// Choices offered for "Default event duration", in minutes.
pub const DURATION_CHOICES: [u32; 6] = [15, 30, 45, 60, 90, 120];

impl Settings {
    /// The first day of the week, with "system" resolved.
    pub fn first_weekday(&self) -> Weekday {
        match self.week_start {
            Some(n) => weekday_from_monday(n),
            None => system_week_start(),
        }
    }

    /// Days shown in the week view (5 or 7).
    pub fn week_days(&self) -> i64 {
        if self.days_per_week == 5 {
            5
        } else {
            7
        }
    }

    /// Makes the date helpers follow these settings (week start, default
    /// event length). Cheap; called every frame.
    pub fn apply(&self) {
        dates::set_week_start(self.first_weekday());
        dates::set_default_event_minutes(self.event_minutes as i64);
    }

    /// Where the day/week grid opens: half an hour before the day starts.
    pub fn scroll_hour(&self) -> f32 {
        (self.day_start as f32 - 0.5).max(0.0)
    }
}

pub fn weekday_from_monday(n: u8) -> Weekday {
    (0..n % 7).fold(Weekday::Mon, |wd, _| wd.succ())
}

pub fn weekday_name(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

pub fn duration_label(minutes: u32) -> String {
    match minutes {
        60 => "1 hour".into(),
        90 => "1.5 hours".into(),
        m if m % 60 == 0 => format!("{} hours", m / 60),
        m => format!("{m} minutes"),
    }
}

/// Choices offered for "Keep past events", in months (0 = all).
pub const HISTORY_CHOICES: [u32; 5] = [0, 60, 24, 12, 6];

pub fn history_label(months: u32) -> String {
    match months {
        0 => "All".into(),
        1 => "1 month".into(),
        12 => "1 year".into(),
        m if m % 12 == 0 => format!("{} years", m / 12),
        m => format!("{m} months"),
    }
}

pub fn refresh_label(minutes: u32) -> String {
    match minutes {
        0 => "Manually".into(),
        1 => "Every minute".into(),
        60 => "Every hour".into(),
        m => format!("Every {m} minutes"),
    }
}

/// The first day of the week from the OS's region settings (Monday if it
/// can't be read). Read once.
pub fn system_week_start() -> Weekday {
    static FIRST: OnceLock<Weekday> = OnceLock::new();
    *FIRST.get_or_init(|| read_system_week_start().unwrap_or(Weekday::Mon))
}

#[cfg(windows)]
fn read_system_week_start() -> Option<Weekday> {
    use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_IFIRSTDAYOFWEEK};
    let mut buf = [0u16; 8];
    // A null locale name means the user's default locale. The answer is a
    // digit: "0" = Monday … "6" = Sunday.
    let n = unsafe { GetLocaleInfoEx(std::ptr::null(), LOCALE_IFIRSTDAYOFWEEK, buf.as_mut_ptr(), buf.len() as i32) };
    if n < 2 {
        return None;
    }
    let digit = char::from_u32(buf[0] as u32)?.to_digit(10)?;
    (digit < 7).then(|| weekday_from_monday(digit as u8))
}

#[cfg(not(windows))]
fn read_system_week_start() -> Option<Weekday> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_their_defaults() {
        let s: Settings = serde_json::from_str(r#"{"days_per_week": 5}"#).unwrap();
        assert_eq!(s.days_per_week, 5);
        assert_eq!(s.hours_shown, 12);
        assert!(s.google_enabled);
    }

    #[test]
    fn labels() {
        assert_eq!(weekday_from_monday(6), Weekday::Sun);
        assert_eq!(duration_label(45), "45 minutes");
        assert_eq!(duration_label(90), "1.5 hours");
        assert_eq!(duration_label(120), "2 hours");
        assert_eq!(refresh_label(0), "Manually");
        assert_eq!(refresh_label(15), "Every 15 minutes");
    }
}
