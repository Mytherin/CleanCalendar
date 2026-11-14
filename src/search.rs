//! Searching events, like the search field in Apple Calendar's toolbar.
//!
//! An event matches when every word of the query appears (ignoring case) in
//! its title, location, notes, or an invitee's name or address. Only shown
//! calendars are searched. A repeating event matches with each of its
//! occurrences, up to as far ahead as the calendars are synced.

use chrono::{Duration, NaiveDate};

use crate::dates::{add_months, first_of_month, midnight};
use crate::model::{Event, Store};
use crate::recurrence;
use crate::sync::MONTHS_AHEAD;

/// The events matching `query`, sorted by start. An empty query matches
/// nothing.
pub fn search(store: &Store, query: &str, today: NaiveDate) -> Vec<Event> {
    let terms: Vec<String> = query.split_whitespace().map(fold).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let until = midnight(add_months(first_of_month(today), MONTHS_AHEAD));
    let mut out = Vec::new();
    for ev in store.events.iter().filter(|e| store.is_visible(&e.calendar_id) && matches(e, &terms)) {
        if ev.recurrence.is_some() {
            let from = ev.start.min(until) - Duration::days(1);
            out.extend(recurrence::occurrences(ev, from, until));
        } else {
            out.push(ev.clone());
        }
    }
    // Lowercase each title once, not twice per comparison.
    out.sort_by_cached_key(|e| (e.start, e.title.to_lowercase()));
    out
}

/// Text for case-insensitive matching. Lowercasing "İ" (Turkish capital
/// I with a dot) gives "i" plus a combining dot, which "istanbul" wouldn't
/// match; the dot is dropped.
fn fold(s: &str) -> String {
    s.to_lowercase().replace('\u{307}', "")
}

/// Whether every term appears somewhere in the event.
fn matches(ev: &Event, terms: &[String]) -> bool {
    let mut fields = vec![fold(&ev.title), fold(&ev.location), fold(&ev.notes)];
    for a in &ev.attendees {
        fields.push(fold(&a.name));
        fields.push(fold(&a.email));
    }
    terms.iter().all(|t| fields.iter().any(|f| f.contains(t.as_str())))
}

/// Remembers the last search, so the results list (drawn every frame)
/// doesn't search every event again on each repaint.
#[derive(Default)]
pub struct Cache {
    key: Option<(String, NaiveDate, u64)>,
    results: Vec<Event>,
    /// How many searches actually ran (for tests).
    pub runs: usize,
}

impl Cache {
    /// The results for `query`, searching again only when the query, the
    /// day, or the events (or which calendars are shown) changed.
    pub fn get(&mut self, store: &Store, query: &str, today: NaiveDate) -> &[Event] {
        let key = (query.to_owned(), today, fingerprint(store));
        if self.key.as_ref() != Some(&key) {
            self.results = search(store, query, today);
            self.key = Some(key);
            self.runs += 1;
        }
        &self.results
    }
}

/// A cheap summary of everything search results depend on. Hashing a few
/// fields per event is far less work than matching them.
fn fingerprint(store: &Store) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in &store.calendars {
        (&c.id, c.visible, store.is_visible(&c.id)).hash(&mut h);
    }
    for e in &store.events {
        (&e.id, e.rev, &e.etag, &e.title, &e.location, e.notes.len(), e.start, e.end, e.attendees.len()).hash(&mut h);
        e.recurrence.is_some().hash(&mut h);
    }
    h.finish()
}

/// Index of the first result on or after `today` (else the last one), to
/// scroll to: like Apple Calendar, the list opens at what's coming up.
pub fn first_upcoming(results: &[Event], today: NaiveDate) -> Option<usize> {
    if results.is_empty() {
        return None;
    }
    Some(results.iter().position(|e| e.end.date() >= today).unwrap_or(results.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Attendee;
    use crate::recurrence::{Freq, Recurrence, RepeatEnd};

    fn day(m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, d).unwrap()
    }

    fn ev(id: &str, title: &str, on: NaiveDate) -> Event {
        Event {
            id: id.into(),
            calendar_id: "local:home".into(),
            title: title.into(),
            start: on.and_hms_opt(9, 0, 0).unwrap(),
            end: on.and_hms_opt(10, 0, 0).unwrap(),
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

    fn ids(v: &[Event]) -> Vec<&str> {
        v.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn every_word_must_appear_somewhere() {
        let mut store = Store::with_defaults();
        let mut a = ev("a", "Dentist", day(11, 20));
        a.location = "Main Street 12".into();
        let mut b = ev("b", "Lunch", day(11, 18));
        b.notes = "Bring the DENTIST forms".into();
        let mut c = ev("c", "Planning", day(11, 19));
        c.attendees.push(Attendee { name: "Hannes Mühleisen".into(), ..Attendee::new("hannes@example.com") });
        store.events = vec![a, b, c];

        assert_eq!(ids(&search(&store, "dentist", day(11, 14))), ["b", "a"], "sorted by start; notes count");
        assert_eq!(ids(&search(&store, "dentist main", day(11, 14))), ["a"]);
        assert_eq!(ids(&search(&store, "MÜHLEISEN", day(11, 14))), ["c"], "invitee names, any case");
        assert_eq!(ids(&search(&store, "example.com", day(11, 14))), ["c"], "invitee addresses");
        assert!(search(&store, "   ", day(11, 14)).is_empty());
        assert!(search(&store, "dentist zebra", day(11, 14)).is_empty());
    }

    #[test]
    fn the_cache_searches_again_only_when_something_changed() {
        let mut store = Store::with_defaults();
        store.events = vec![ev("a", "Standup", day(11, 20))];
        let mut cache = Cache::default();
        for _ in 0..5 {
            assert_eq!(cache.get(&store, "stand", day(11, 1)).len(), 1);
        }
        assert_eq!(cache.runs, 1, "repaints reuse the results");

        store.events[0].title = "Retro".into();
        assert!(cache.get(&store, "stand", day(11, 1)).is_empty(), "an edit is noticed");
        store.events.push(ev("b", "Standup again", day(11, 21)));
        assert_eq!(cache.get(&store, "stand", day(11, 1)).len(), 1, "a new event is noticed");
        assert_eq!(cache.get(&store, "retro", day(11, 1)).len(), 1, "a new query is noticed");
        store.calendars[0].visible = false;
        assert!(cache.get(&store, "retro", day(11, 1)).is_empty(), "hiding a calendar is noticed");
        assert_eq!(cache.runs, 5);
    }

    #[test]
    fn dotted_capitals_match_their_plain_letters() {
        let mut store = Store::with_defaults();
        store.events = vec![ev("a", "Flight to İstanbul", day(11, 20))];
        assert_eq!(search(&store, "istanbul", day(11, 1)).len(), 1);
        assert_eq!(search(&store, "İSTANBUL", day(11, 1)).len(), 1);
    }

    #[test]
    fn hidden_calendars_are_not_searched() {
        let mut store = Store::with_defaults();
        store.events = vec![ev("a", "Standup", day(11, 20))];
        store.calendars.iter_mut().find(|c| c.id == "local:home").unwrap().visible = false;
        assert!(search(&store, "standup", day(11, 14)).is_empty());
    }

    #[test]
    fn repeating_events_list_each_occurrence_until_the_sync_horizon() {
        let mut store = Store::with_defaults();
        let mut weekly = ev("w", "Gym", day(1, 5));
        weekly.recurrence = Some(Recurrence { end: RepeatEnd::Never, ..Recurrence::new(Freq::Weekly) });
        let mut monthly = ev("m", "Gym", day(3, 1));
        let mut rule = Recurrence { end: RepeatEnd::Count(3), ..Recurrence::new(Freq::Monthly) };
        rule.exceptions.push(day(4, 1).and_hms_opt(9, 0, 0).unwrap());
        monthly.recurrence = Some(rule);
        store.events = vec![weekly, monthly];

        let found = search(&store, "gym", day(11, 14));
        let weekly: Vec<_> = found.iter().filter(|e| e.id.starts_with("w#")).collect();
        assert_eq!(weekly.first().unwrap().start.date(), day(1, 5), "from the series' start");
        assert!(weekly.last().unwrap().start.date() < NaiveDate::from_ymd_opt(2027, 12, 1).unwrap());
        assert!(weekly.last().unwrap().start.date() >= NaiveDate::from_ymd_opt(2027, 11, 20).unwrap());
        let monthly: Vec<_> = found.iter().filter(|e| e.id.starts_with("m#")).map(|e| e.start.date()).collect();
        assert_eq!(monthly, [day(3, 1), day(5, 1)], "the removed occurrence is left out");
        assert!(found.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[test]
    fn opens_at_what_is_coming_up() {
        let evs = vec![ev("a", "x", day(1, 1)), ev("b", "x", day(11, 14)), ev("c", "x", day(12, 1))];
        assert_eq!(first_upcoming(&evs, day(11, 14)), Some(1));
        assert_eq!(first_upcoming(&evs, day(12, 31)), Some(2), "nothing ahead: the last one");
        assert_eq!(first_upcoming(&[], day(1, 1)), None);
    }
}
