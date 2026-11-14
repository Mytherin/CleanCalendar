//! iCalendar parsing, expansion and writing. Times are checked in the
//! event's own zone (or UTC), so the tests pass whatever zone the computer
//! running them is in.

use chrono::{Datelike, NaiveDate, Timelike, Weekday};

use super::*;
use crate::recurrence::{Freq, Recurrence, RepeatEnd};

fn cal() -> CalRef {
    CalRef::new("caldav-test", "https://dav.example.com/cal/home/")
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn t(y: i32, m: u32, day: u32, h: u32, min: u32) -> NaiveDateTime {
    d(y, m, day).and_hms_opt(h, min, 0).unwrap()
}

fn ny() -> Tz {
    "America/New_York".parse().unwrap()
}

/// An event's start as wall time in `tz`.
fn wall(ev: &Event, tz: Tz) -> NaiveDateTime {
    recurrence::to_zone(ev.start, Some(tz))
}

fn ics(body: &str) -> Component {
    let text = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\n{body}END:VCALENDAR\r\n");
    parse(&text).unwrap()
}

fn year() -> (NaiveDate, NaiveDate) {
    (d(2026, 1, 1), d(2027, 12, 31))
}

const WEEKLY: &str = "BEGIN:VEVENT\r\nUID:standup@example.com\r\nSUMMARY:Standup\r\n\
    DTSTART;TZID=America/New_York:20261102T090000\r\nDTEND;TZID=America/New_York:20261102T091500\r\n\
    RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=6\r\nEND:VEVENT\r\n";

// ---- content lines ------------------------------------------------------------

#[test]
fn long_lines_fold_and_unfold_without_splitting_characters() {
    let mut c = Component::new("VEVENT");
    let text = "Ünïcödé ".repeat(20) + "and a comma, a semicolon; a backslash \\ and\na newline";
    c.set_text("DESCRIPTION", &text);
    let mut root = new_calendar();
    root.subs.push(c);
    let out = write(&root);
    for line in out.split("\r\n") {
        assert!(line.len() <= 75, "line too long ({}): {line}", line.len());
    }
    let back = parse(&out).unwrap();
    assert_eq!(back.subs[0].text("DESCRIPTION").unwrap(), text);
}

#[test]
fn parameters_with_quotes_and_lists_are_read() {
    let p = parse_line(r#"ATTENDEE;CN="Doe, Jane";ROLE=REQ-PARTICIPANT;DELEGATED-FROM="mailto:a@x","mailto:b@x":mailto:jane@x"#)
        .unwrap();
    assert_eq!(p.name, "ATTENDEE");
    assert_eq!(p.param("CN"), Some("Doe, Jane"));
    // Several quoted values stay quoted one by one, to be written back as they were.
    assert_eq!(p.param("DELEGATED-FROM"), Some(r#""mailto:a@x","mailto:b@x""#));
    assert_eq!(p.value, "mailto:jane@x");
    let mut c = Component::new("VEVENT");
    c.props.push(p);
    let unfolded = write(&c).replace("\r\n ", "");
    assert!(unfolded.contains(r#"CN="Doe, Jane";ROLE=REQ-PARTICIPANT;DELEGATED-FROM="mailto:a@x","mailto:b@x":mailto"#), "{unfolded}");
    let p = parse_line("dtstart;tzid=Europe/Paris:20261114T090000").unwrap();
    assert_eq!((p.name.as_str(), p.param("TZID")), ("DTSTART", Some("Europe/Paris")));
    assert!(parse_line("no colon here").is_none());
}

#[test]
fn lf_only_files_and_tab_continuations_parse() {
    let c = parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Lunch with\n\t the team\nEND:VEVENT\nEND:VCALENDAR\n").unwrap();
    // Only the one whitespace character that marks the continuation goes.
    assert_eq!(c.subs[0].text("SUMMARY").as_deref(), Some("Lunch with the team"));
    assert!(parse("BEGIN:VCALENDAR\r\n").is_err(), "unterminated");
    assert!(parse("garbage").is_err());
}

#[test]
fn time_zone_ids_with_vendor_prefixes_are_understood() {
    assert_eq!(parse_tzid("America/New_York"), Some(ny()));
    assert_eq!(parse_tzid("/mozilla.org/20050126_1/America/New_York"), Some(ny()));
    assert_eq!(parse_tzid("\"Europe/Paris\""), Some("Europe/Paris".parse().unwrap()));
    assert_eq!(parse_tzid("Custom Zone 1"), None);
}

#[test]
fn windows_zone_names_from_outlook_invitations_are_understood() {
    assert_eq!(parse_tzid("W. Europe Standard Time"), Some("Europe/Berlin".parse().unwrap()));
    assert_eq!(parse_tzid("eastern standard time"), Some(ny()));
    let c = ics("BEGIN:VEVENT\r\nUID:o\r\nSUMMARY:From Outlook\r\nDTSTART;TZID=Eastern Standard Time:20261116T090000\r\n\
        DTEND;TZID=Eastern Standard Time:20261116T100000\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/o.ics", None, &c, year().0, year().1);
    assert_eq!(wall(&evs[0], ny()), t(2026, 11, 16, 9, 0), "not floating");
    // Occurrences are named in the file's own TZID, which it defines.
    let mut c2 = c.clone();
    remove_occurrence(&mut c2, "20261117T090000").unwrap();
    let ex = master(&c2).unwrap().prop("EXDATE").unwrap().clone();
    assert_eq!(ex.param("TZID"), Some("Eastern Standard Time"));
    assert!(c2.subs.iter().any(|s| s.name == "VTIMEZONE" && s.text("TZID").as_deref() == Some("Eastern Standard Time")));
    assert_eq!(events(&cal(), "/o.ics", None, &c2, year().0, year().1).len(), 2);
}

#[test]
fn unreadable_lines_are_kept_as_they_were() {
    let c = ics("BEGIN:VEVENT\r\nUID:r\r\nSUMMARY:x\r\nX-ODD;FOO=\"unclosed:value\r\nDTSTART:20261114T090000Z\r\nEND:VEVENT\r\n");
    let ev = events(&cal(), "/r.ics", None, &c, year().0, year().1).remove(0);
    let out = write(&put(Some(c), &ev, "r", None).unwrap());
    assert!(out.contains("X-ODD;FOO=\"unclosed:value\r\n"), "{out}");
}

#[test]
fn a_new_location_drops_apples_map_pin() {
    let c = ics("BEGIN:VEVENT\r\nUID:l\r\nLOCATION:Old cafe\r\nX-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-TITLE=Old cafe:geo:1,2\r\n\
        DTSTART:20261114T090000Z\r\nEND:VEVENT\r\n");
    let mut ev = events(&cal(), "/l.ics", None, &c, year().0, year().1).remove(0);
    let same = put(Some(c.clone()), &ev, "l", None).unwrap();
    assert!(master(&same).unwrap().prop("X-APPLE-STRUCTURED-LOCATION").is_some(), "kept while the location is unchanged");
    ev.location = "New bar".into();
    let moved = put(Some(c), &ev, "l", None).unwrap();
    assert!(master(&moved).unwrap().prop("X-APPLE-STRUCTURED-LOCATION").is_none());
}

#[test]
fn renaming_leaves_floating_and_unknown_zone_times_alone() {
    for dtstart in ["DTSTART:20261201T090000", "DTSTART;TZID=Customized Time Zone:20261201T090000"] {
        let c = ics(&format!("BEGIN:VEVENT\r\nUID:f\r\nSUMMARY:Standup\r\n{dtstart}\r\nDURATION:PT30M\r\nEND:VEVENT\r\n"));
        let mut ev = events(&cal(), "/cal/home/f.ics", Some("\"e1\""), &c, year().0, year().1).remove(0);
        ev.base = Some(ev.fields());
        ev.title = "Standup (moved room)".into();
        let out = write(&put(Some(c), &ev, "f", None).unwrap());
        assert!(out.contains(&format!("{dtstart}\r\n")), "{dtstart} kept as it was:\n{out}");
        assert!(out.contains("DURATION:PT30M") && !out.contains("DTEND"), "{out}");
        assert!(out.contains("SUMMARY:Standup (moved room)"));
    }
}

#[test]
fn moving_a_floating_event_keeps_it_floating() {
    let c = ics("BEGIN:VEVENT\r\nUID:f\r\nSUMMARY:Standup\r\nDTSTART:20261201T090000\r\nDTEND:20261201T093000\r\nEND:VEVENT\r\n");
    let mut ev = events(&cal(), "/cal/home/f.ics", None, &c, year().0, year().1).remove(0);
    ev.base = Some(ev.fields());
    ev.start += Duration::hours(1);
    ev.end += Duration::hours(1);
    let out = write(&put(Some(c), &ev, "f", None).unwrap());
    assert!(out.contains("DTSTART:20261201T100000\r\n"), "{out}");
    assert!(out.contains("DTEND:20261201T103000\r\n"), "{out}");
}

#[test]
fn a_character_split_by_another_clients_fold_is_joined_back() {
    // "café" folded between the two bytes of "é" (C3 A9).
    let data = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:x\r\n\
        DTSTART:20261114T090000Z\r\nDESCRIPTION:caf\xC3\r\n \xA9 au lait\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let c = parse_bytes(data).unwrap();
    let ev = c.events().next().unwrap();
    assert_eq!(ev.text("DESCRIPTION").as_deref(), Some("café au lait"));
}

#[test]
fn durations() {
    assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
    assert_eq!(parse_duration("P2D"), Some(Duration::days(2)));
    assert_eq!(parse_duration("P1W"), Some(Duration::weeks(1)));
    assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
    assert_eq!(parse_duration("P1DT12H"), Some(Duration::hours(36)));
    assert_eq!(parse_duration("1H"), None);
    // Out of range: invalid, not a panic.
    assert_eq!(parse_duration("P100000000000000W"), None);
    assert_eq!(parse_duration("P99999999999999999999D"), None);
}

#[test]
fn an_absurd_duration_from_the_server_does_not_panic() {
    let c = ics("BEGIN:VEVENT\r\nUID:x\r\nSUMMARY:Broken\r\nDTSTART:20260101T090000Z\r\nDURATION:P100000000D\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/cal/home/x.ics", None, &c, year().0, year().1);
    assert_eq!(evs.len(), 1, "shown, with no end beyond its start");
    assert_eq!(evs[0].duration(), Duration::zero());
}

// ---- reading -----------------------------------------------------------------------

#[test]
fn a_plain_zoned_event() {
    let c = ics("BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Dentist\r\nLOCATION:Main St\\, 4\r\nDESCRIPTION:Bring card\\nand ID\r\n\
        DTSTART;TZID=America/New_York:20261114T090000\r\nDTEND;TZID=America/New_York:20261114T100000\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/cal/home/a.ics", Some("\"e1\""), &c, year().0, year().1);
    assert_eq!(evs.len(), 1);
    let ev = &evs[0];
    assert_eq!(ev.id, "caldav-test:https://dav.example.com/cal/home/:/cal/home/a.ics");
    assert_eq!(ev.calendar_id, cal().local_id());
    assert_eq!((ev.title.as_str(), ev.location.as_str(), ev.notes.as_str()), ("Dentist", "Main St, 4", "Bring card\nand ID"));
    assert_eq!(ev.time_zone.as_deref(), Some("America/New_York"));
    assert_eq!(wall(ev, ny()), t(2026, 11, 14, 9, 0));
    assert_eq!(ev.duration(), Duration::hours(1));
    assert_eq!((ev.remote_id.as_deref(), ev.recurring_id.as_deref()), (Some("/cal/home/a.ics"), None));
    assert_eq!(ev.etag.as_deref(), Some("\"e1\""));
}

#[test]
fn all_day_utc_floating_and_duration_forms() {
    let c = ics("BEGIN:VEVENT\r\nUID:trip\r\nSUMMARY:Trip\r\nDTSTART;VALUE=DATE:20261111\r\nDTEND;VALUE=DATE:20261114\r\nEND:VEVENT\r\n");
    let ev = &events(&cal(), "/t.ics", None, &c, year().0, year().1)[0];
    assert!(ev.all_day);
    assert_eq!((ev.start, ev.end), (t(2026, 11, 11, 0, 0), t(2026, 11, 14, 0, 0)), "end stays exclusive");
    assert_eq!(ev.time_zone, None);

    // A date with no end lasts one day.
    let c = ics("BEGIN:VEVENT\r\nUID:b\r\nDTSTART;VALUE=DATE:20261111\r\nEND:VEVENT\r\n");
    let ev = &events(&cal(), "/b.ics", None, &c, year().0, year().1)[0];
    assert_eq!(ev.end - ev.start, Duration::days(1));
    assert_eq!(ev.title, "(No title)");

    let c = ics("BEGIN:VEVENT\r\nUID:u\r\nDTSTART:20261114T170000Z\r\nDURATION:PT45M\r\nEND:VEVENT\r\n");
    let ev = &events(&cal(), "/u.ics", None, &c, year().0, year().1)[0];
    assert_eq!(recurrence::to_zone(ev.start, Some(chrono_tz::UTC)), t(2026, 11, 14, 17, 0));
    assert_eq!(ev.duration(), Duration::minutes(45));
    assert_eq!(ev.time_zone, None, "UTC times show in local time");

    let c = ics("BEGIN:VEVENT\r\nUID:f\r\nDTSTART:20261114T080000\r\nDTEND:20261114T083000\r\nEND:VEVENT\r\n");
    let ev = &events(&cal(), "/f.ics", None, &c, year().0, year().1)[0];
    assert_eq!(ev.start, t(2026, 11, 14, 8, 0), "floating: the same wall time here");
}

#[test]
fn a_series_becomes_occurrences_of_a_fixed_series() {
    let c = ics(WEEKLY);
    let evs = events(&cal(), "/s.ics", Some("\"v1\""), &c, year().0, year().1);
    assert_eq!(evs.len(), 6, "COUNT=6");
    let days: Vec<(u32, Weekday)> = evs.iter().map(|e| (wall(e, ny()).day(), wall(e, ny()).weekday())).collect();
    assert_eq!(days, [(2, Weekday::Mon), (4, Weekday::Wed), (9, Weekday::Mon), (11, Weekday::Wed), (16, Weekday::Mon), (18, Weekday::Wed)]);
    for e in &evs {
        assert_eq!(e.recurring_id.as_deref(), Some("/s.ics"));
        assert!(e.recurrence.is_none(), "not editable as a series here");
        assert!(e.remote_id.as_deref().unwrap().starts_with("/s.ics#2026"));
        assert_eq!(e.duration(), Duration::minutes(15));
        assert_eq!(e.etag.as_deref(), Some("\"v1\""));
    }
    assert_eq!(evs[0].remote_id.as_deref(), Some("/s.ics#20261102T090000"));
    // The series' wall time holds across the DST change (1 November 2026 in NY
    // is before the first; check one in March instead).
    let c = ics("BEGIN:VEVENT\r\nUID:dst\r\nDTSTART;TZID=America/New_York:20270301T090000\r\nDTEND;TZID=America/New_York:20270301T100000\r\n\
        RRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/dst.ics", None, &c, year().0, year().1);
    assert!(evs.iter().all(|e| wall(e, ny()).hour() == 9), "09:00 in New York before and after 14 March");
    let utc_hours: Vec<u32> = evs.iter().map(|e| recurrence::to_zone(e.start, Some(chrono_tz::UTC)).hour()).collect();
    assert_eq!(utc_hours, [14, 14, 13]);
}

#[test]
fn only_occurrences_inside_the_window_are_listed() {
    let c = ics("BEGIN:VEVENT\r\nUID:d\r\nDTSTART;VALUE=DATE:20200101\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/d.ics", None, &c, d(2026, 11, 1), d(2026, 11, 8));
    let first = evs.first().unwrap().start.date();
    let last = evs.last().unwrap().start.date();
    assert!(first >= d(2026, 10, 30) && last <= d(2026, 11, 10), "{first}..{last}");
    assert!(evs.len() >= 7 && evs.len() <= 11);
}

#[test]
fn rules_the_editor_cannot_express_still_expand() {
    // The second Tuesday of every month, ending on a date.
    let c = ics("BEGIN:VEVENT\r\nUID:m\r\nDTSTART;TZID=America/New_York:20261110T180000\r\nDTEND;TZID=America/New_York:20261110T190000\r\n\
        RRULE:FREQ=MONTHLY;BYDAY=2TU;UNTIL=20270301T000000Z\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/m.ics", None, &c, year().0, year().1);
    let dates: Vec<NaiveDate> = evs.iter().map(|e| wall(e, ny()).date()).collect();
    assert_eq!(dates, [d(2026, 11, 10), d(2026, 12, 8), d(2027, 1, 12), d(2027, 2, 9)]);

    // UNTIL as a plain date includes that day.
    let c = ics("BEGIN:VEVENT\r\nUID:u\r\nDTSTART;VALUE=DATE:20261228\r\nRRULE:FREQ=DAILY;UNTIL=20261231\r\nEND:VEVENT\r\n");
    assert_eq!(events(&cal(), "/u.ics", None, &c, year().0, year().1).len(), 4);
}

#[test]
fn exdates_overrides_and_cancelled_occurrences() {
    let c = ics(&format!(
        "{}BEGIN:VEVENT\r\nUID:standup@example.com\r\nRECURRENCE-ID;TZID=America/New_York:20261104T090000\r\n\
         SUMMARY:Standup (moved)\r\nDTSTART;TZID=America/New_York:20261104T110000\r\nDTEND;TZID=America/New_York:20261104T113000\r\nEND:VEVENT\r\n\
         BEGIN:VEVENT\r\nUID:standup@example.com\r\nRECURRENCE-ID:20261109T140000Z\r\nSTATUS:CANCELLED\r\n\
         DTSTART;TZID=America/New_York:20261109T090000\r\nEND:VEVENT\r\n",
        WEEKLY.replace("COUNT=6\r\n", "COUNT=6\r\nEXDATE;TZID=America/New_York:20261116T090000\r\n")
    ));
    let evs = events(&cal(), "/s.ics", None, &c, year().0, year().1);
    let summary: Vec<(u32, u32, &str)> =
        evs.iter().map(|e| (wall(e, ny()).day(), wall(e, ny()).hour(), e.title.as_str())).collect();
    // 2nd: normal. 4th: moved to 11:00. 9th: cancelled (named in UTC). 11th:
    // normal. 16th: excluded. 18th: normal.
    assert_eq!(summary, [(2, 9, "Standup"), (4, 11, "Standup (moved)"), (11, 9, "Standup"), (18, 9, "Standup")]);
    let moved = &evs[1];
    assert_eq!(moved.remote_id.as_deref(), Some("/s.ics#20261104T090000"), "keyed by its original start");
    assert_eq!(moved.duration(), Duration::minutes(30));
}

#[test]
fn find_returns_one_occurrence_or_nothing() {
    let c = ics(WEEKLY);
    let ev = find(&cal(), "/s.ics#20261109T090000", None, &c).unwrap();
    assert_eq!(wall(&ev, ny()), t(2026, 11, 9, 9, 0));
    assert_eq!(ev.recurring_id.as_deref(), Some("/s.ics"));
    assert!(find(&cal(), "/s.ics#20261110T090000", None, &c).is_none(), "not an occurrence");
    assert!(find(&cal(), "/s.ics", None, &c).is_none(), "a series isn't one event");
    let plain = ics("BEGIN:VEVENT\r\nUID:p\r\nDTSTART:20261114T090000Z\r\nEND:VEVENT\r\n");
    assert!(find(&cal(), "/p.ics", None, &plain).is_some());
}

#[test]
fn remote_ids_split_into_resource_and_occurrence() {
    assert_eq!(split_remote_id("/a/b.ics"), ("/a/b.ics", None));
    assert_eq!(split_remote_id("/a/b.ics#20261114"), ("/a/b.ics", Some("20261114")));
    assert_eq!(key(t(2026, 11, 14, 9, 0), false), "20261114T090000");
    assert_eq!(key(t(2026, 11, 14, 0, 0), true), "20261114");
    assert_eq!(parse_key("20261114"), Some(t(2026, 11, 14, 0, 0)));
}

// ---- writing --------------------------------------------------------------------------

fn app_event(title: &str) -> Event {
    let mut ev = crate::model::tests::event("local:abc123", &cal().local_id(), title);
    ev.time_zone = Some("America/New_York".into());
    ev.start = recurrence::from_zone(t(2026, 11, 14, 9, 0), Some(ny()));
    ev.end = recurrence::from_zone(t(2026, 11, 14, 10, 30), Some(ny()));
    ev.location = "Room 1".into();
    ev.notes = "line 1\nline 2".into();
    ev
}

#[test]
fn a_new_event_round_trips() {
    let ev = app_event("Planning; Q1, Q2");
    let c = put(None, &ev, "abc123", None).unwrap();
    let text = write(&c);
    assert!(text.contains("UID:abc123"));
    assert!(text.contains("DTSTART;TZID=America/New_York:20261114T090000"), "{text}");
    assert!(text.contains("BEGIN:VTIMEZONE\r\nTZID:America/New_York"), "a VTIMEZONE for the TZID");
    assert!(text.find("BEGIN:VTIMEZONE").unwrap() < text.find("BEGIN:VEVENT").unwrap());

    let back = &events(&cal(), "/x.ics", None, &parse(&text).unwrap(), year().0, year().1)[0];
    assert_eq!(back.fields(), ev.fields());
}

#[test]
fn all_day_and_unpinned_events_are_written_as_dates_and_utc() {
    let mut ev = app_event("Holiday");
    ev.all_day = true;
    ev.time_zone = None;
    ev.start = t(2026, 12, 24, 0, 0);
    ev.end = t(2026, 12, 27, 0, 0);
    let text = write(&put(None, &ev, "h", None).unwrap());
    assert!(text.contains("DTSTART;VALUE=DATE:20261224") && text.contains("DTEND;VALUE=DATE:20261227"), "{text}");
    assert!(!text.contains("VTIMEZONE"));

    let mut ev = app_event("Call");
    ev.time_zone = None;
    let text = write(&put(None, &ev, "c", None).unwrap());
    let dtstart = text.lines().find(|l| l.starts_with("DTSTART")).unwrap();
    assert!(dtstart.ends_with('Z'), "an instant: {dtstart}");
    let back = &events(&cal(), "/c.ics", None, &parse(&text).unwrap(), year().0, year().1)[0];
    assert_eq!((back.start, back.end), (ev.start, ev.end));
}

#[test]
fn a_new_series_carries_its_rule() {
    let mut ev = app_event("Gym");
    let mut rule = Recurrence::new(Freq::Weekly);
    rule.weekdays = vec![Weekday::Sat, Weekday::Sun];
    rule.end = RepeatEnd::Count(4);
    ev.recurrence = Some(rule);
    let c = put(None, &ev, "gym", None).unwrap();
    assert!(write(&c).contains("RRULE:FREQ=WEEKLY;BYDAY=SA,SU;COUNT=4"));
    let evs = events(&cal(), "/gym.ics", None, &c, year().0, year().1);
    let days: Vec<u32> = evs.iter().map(|e| wall(e, ny()).day()).collect();
    assert_eq!(days, [14, 15, 21, 22]);
}

#[test]
fn updating_keeps_what_the_app_does_not_model() {
    let original = ics(
        "BEGIN:VEVENT\r\nUID:keep\r\nSUMMARY:Old\r\nSEQUENCE:3\r\nATTENDEE;CN=Jane:mailto:jane@x\r\nX-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\n\
         DTSTART;TZID=America/New_York:20261114T090000\r\nDURATION:PT1H\r\n\
         BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
    );
    let mut ev = events(&cal(), "/keep.ics", None, &original, year().0, year().1).remove(0);
    ev.title = "New".into();
    ev.location = String::new();
    let c = put(Some(original), &ev, "ignored", None).unwrap();
    let v = &c.subs.iter().find(|s| s.name == "VEVENT").unwrap();
    assert_eq!(v.text("SUMMARY").as_deref(), Some("New"));
    assert_eq!(v.text("UID").as_deref(), Some("keep"), "the UID never changes");
    assert_eq!(v.prop("SEQUENCE").unwrap().value, "4");
    assert!(v.prop("ATTENDEE").is_some() && v.prop("X-APPLE-TRAVEL-ADVISORY-BEHAVIOR").is_some());
    assert_eq!(v.subs.len(), 1, "the alarm is kept");
    assert!(v.prop("DURATION").is_none() && v.prop("DTEND").is_some(), "DTEND replaces DURATION");
    assert!(v.prop("LOCATION").is_none(), "emptied fields are removed");
}

#[test]
fn editing_one_occurrence_writes_an_override() {
    let original = ics(WEEKLY);
    let mut occ = find(&cal(), "/s.ics#20261109T090000", None, &original).unwrap();
    occ.title = "Standup (with guests)".into();
    occ.start += Duration::hours(1);
    occ.end += Duration::hours(1);
    let c = put(Some(original.clone()), &occ, "standup@example.com", Some("20261109T090000")).unwrap();

    let vevents: Vec<&Component> = c.subs.iter().filter(|s| s.name == "VEVENT").collect();
    assert_eq!(vevents.len(), 2);
    assert_eq!(vevents[0], original.subs.iter().find(|s| s.name == "VEVENT").unwrap(), "the series is untouched");
    let o = vevents[1];
    let rid = o.prop("RECURRENCE-ID").unwrap();
    assert_eq!((rid.param("TZID"), rid.value.as_str()), (Some("America/New_York"), "20261109T090000"), "written like DTSTART");
    assert!(o.prop("RRULE").is_none());
    assert_eq!(o.text("UID").as_deref(), Some("standup@example.com"));

    let evs = events(&cal(), "/s.ics", None, &c, year().0, year().1);
    assert_eq!(evs.len(), 6);
    let edited = evs.iter().find(|e| e.remote_id.as_deref() == Some("/s.ics#20261109T090000")).unwrap();
    assert_eq!((edited.title.as_str(), wall(edited, ny()).hour()), ("Standup (with guests)", 10));

    // Editing it again updates the same override.
    let mut again = edited.clone();
    again.title = "Standup (final)".into();
    let c2 = put(Some(c), &again, "x", Some("20261109T090000")).unwrap();
    assert_eq!(c2.subs.iter().filter(|s| s.name == "VEVENT").count(), 2);
    assert!(find(&cal(), "/s.ics#20261109T090000", None, &c2).unwrap().title.ends_with("(final)"));
}

#[test]
fn removing_an_occurrence_adds_an_exdate_and_drops_its_override() {
    let mut c = ics(WEEKLY);
    let occ = find(&cal(), "/s.ics#20261104T090000", None, &c).unwrap();
    c = put(Some(c), &occ, "x", Some("20261104T090000")).unwrap();
    assert!(remove_occurrence(&mut c, "20261104T090000").unwrap());
    assert_eq!(c.subs.iter().filter(|s| s.name == "VEVENT").count(), 1, "override gone");
    let master = master(&c).unwrap();
    let ex = master.prop("EXDATE").unwrap();
    assert_eq!((ex.param("TZID"), ex.value.as_str()), (Some("America/New_York"), "20261104T090000"));
    assert_eq!(events(&cal(), "/s.ics", None, &c, year().0, year().1).len(), 5);
    assert!(find(&cal(), "/s.ics#20261104T090000", None, &c).is_none());

    // An invitation to just one occurrence has no master: removing it
    // leaves nothing.
    let mut lone = ics("BEGIN:VEVENT\r\nUID:inv\r\nRECURRENCE-ID:20261114T150000Z\r\nDTSTART:20261114T150000Z\r\nEND:VEVENT\r\n");
    assert!(!remove_occurrence(&mut lone, "20261114T150000").unwrap());
}

#[test]
fn all_day_series_occurrences_use_dates() {
    let mut c = ics("BEGIN:VEVENT\r\nUID:bday\r\nSUMMARY:Birthday\r\nDTSTART;VALUE=DATE:20261120\r\nRRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n");
    let evs = events(&cal(), "/b.ics", None, &c, year().0, year().1);
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].remote_id.as_deref(), Some("/b.ics#20261120"));
    assert!(evs.iter().all(|e| e.all_day && e.duration() == Duration::days(1)));
    remove_occurrence(&mut c, "20271120").unwrap();
    let ex = master(&c).unwrap().prop("EXDATE").unwrap().clone();
    assert_eq!((ex.param("VALUE"), ex.value.as_str()), (Some("DATE"), "20271120"));
    assert_eq!(events(&cal(), "/b.ics", None, &c, year().0, year().1).len(), 1);
}

#[test]
fn time_zone_definitions_are_generated_from_the_zone_database() {
    let z = vtimezone(ny(), "America/New_York", 2026);
    assert_eq!(z.text("TZID").as_deref(), Some("America/New_York"));
    let dst: Vec<&Component> = z.subs.iter().filter(|s| s.name == "DAYLIGHT").collect();
    let std: Vec<&Component> = z.subs.iter().filter(|s| s.name == "STANDARD").collect();
    assert!(dst.len() >= 20 && std.len() >= 20, "one change each way per year");
    let mar_2026 = dst.iter().find(|s| s.prop("DTSTART").unwrap().value.starts_with("2026")).unwrap();
    assert_eq!(mar_2026.prop("DTSTART").unwrap().value, "20260308T020000", "local time before the change");
    assert_eq!(mar_2026.prop("TZOFFSETFROM").unwrap().value, "-0500");
    assert_eq!(mar_2026.prop("TZOFFSETTO").unwrap().value, "-0400");
    assert_eq!(mar_2026.prop("TZNAME").unwrap().value, "EDT");

    let tokyo = vtimezone("Asia/Tokyo".parse().unwrap(), "Asia/Tokyo", 2026);
    assert_eq!(tokyo.subs.len(), 1, "no changes: one fixed offset");
    assert_eq!(tokyo.subs[0].prop("TZOFFSETTO").unwrap().value, "+0900");

    let kolkata = vtimezone("Asia/Kolkata".parse().unwrap(), "Asia/Kolkata", 2026);
    assert_eq!(kolkata.subs[0].prop("TZOFFSETTO").unwrap().value, "+0530");
}
