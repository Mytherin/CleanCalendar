//! iCalendar (RFC 5545) as CalDAV stores it, mapped to the app's events.
//!
//! A CalDAV *resource* is one `.ics` file holding one event: a VEVENT (the
//! "master"), and for a repeating event any number of override VEVENTs, each
//! with a `RECURRENCE-ID` naming the occurrence it replaces.
//!
//! The app shows repeating CalDAV events the way it shows Google's: as
//! separate occurrences of a series it doesn't edit as a whole (see
//! `Editor::in_fixed_series`). Each occurrence has the remote id
//! `<href>#<key>` and the series id `<href>` as `recurring_id`, where `<key>`
//! is its original start in series time (see [`key`]). Editing one writes an
//! override, deleting one adds an `EXDATE`, deleting the series deletes the
//! resource. A plain event's remote id is just `<href>`.
//!
//! Everything the app doesn't model (alarms, attendees, categories, other
//! clients' X- properties) is kept as it was: writing starts from the
//! server's copy and only replaces the properties the app owns.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::{OffsetComponents, OffsetName, Tz};

use crate::dates::midnight;
use crate::model::Event;
use crate::recurrence::{self, convert};
use crate::remote::CalRef;

const PRODID: &str = "-//CleanCalendar//CleanCalendar//EN";
const STAMP: &str = "%Y%m%dT%H%M%S";
const DATE: &str = "%Y%m%d";

// ---------------------------------------------------------------------------
// Content lines and components

#[derive(Clone, Debug, PartialEq)]
pub struct Prop {
    /// Upper case.
    pub name: String,
    /// Names upper case; values without quotes.
    pub params: Vec<(String, String)>,
    /// As written (TEXT values still escaped).
    pub value: String,
    /// A line that couldn't be parsed, kept verbatim.
    pub raw: Option<String>,
}

impl Prop {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Prop { name: name.to_owned(), params: Vec::new(), value: value.into(), raw: None }
    }

    pub fn with(mut self, param: &str, value: &str) -> Self {
        self.params.push((param.to_owned(), value.to_owned()));
        self
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        unescape_text(&self.value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    /// Upper case: "VCALENDAR", "VEVENT", …
    pub name: String,
    pub props: Vec<Prop>,
    pub subs: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Component { name: name.to_owned(), props: Vec::new(), subs: Vec::new() }
    }

    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Prop> {
        self.props.iter().filter(move |p| p.name == name)
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name).map(Prop::text)
    }

    pub fn remove(&mut self, name: &str) {
        self.props.retain(|p| p.name != name);
    }

    /// Replaces every property of this name with `p` (in the first one's place).
    pub fn set(&mut self, p: Prop) {
        match self.props.iter().position(|q| q.name == p.name) {
            Some(i) => {
                let name = p.name.clone();
                self.props[i] = p;
                let mut seen = false;
                self.props.retain(|q| {
                    if q.name != name {
                        return true;
                    }
                    let keep = !seen;
                    seen = true;
                    keep
                });
            }
            None => self.props.push(p),
        }
    }

    /// Sets a TEXT property, or removes it when empty.
    fn set_text(&mut self, name: &str, value: &str) {
        if value.is_empty() {
            self.remove(name);
        } else {
            self.set(Prop::new(name, escape_text(value)));
        }
    }

    fn events(&self) -> impl Iterator<Item = &Component> {
        self.subs.iter().filter(|c| c.name == "VEVENT")
    }
}

pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

pub fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
    }
    out
}

/// Parses one unfolded content line: `NAME;P=v;Q="a:b":value`.
fn parse_line(line: &str) -> Option<Prop> {
    let mut name_end = None;
    for (i, c) in line.char_indices() {
        if c == ';' || c == ':' {
            name_end = Some(i);
            break;
        }
    }
    let name_end = name_end?;
    let name = line[..name_end].trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let mut params = Vec::new();
    let mut rest = &line[name_end..];
    while let Some(after) = rest.strip_prefix(';') {
        let eq = after.find('=')?;
        let pname = after[..eq].trim().to_ascii_uppercase();
        let mut v = &after[eq + 1..];
        let mut values = Vec::new();
        let mut quoted = false;
        loop {
            if let Some(q) = v.strip_prefix('"') {
                let close = q.find('"')?;
                values.push(q[..close].to_owned());
                quoted = true;
                v = &q[close + 1..];
            } else {
                let end = v.find([',', ';', ':']).unwrap_or(v.len());
                values.push(v[..end].to_owned());
                v = &v[end..];
            }
            match v.strip_prefix(',') {
                Some(more) => v = more,
                None => break,
            }
        }
        // Several quoted values ("a","b") are kept quoted one by one, so
        // they're written back as they were.
        let value = if values.len() > 1 && quoted {
            values.iter().map(|x| format!("\"{x}\"")).collect::<Vec<_>>().join(",")
        } else {
            values.join(",")
        };
        params.push((pname, value));
        rest = v;
    }
    let value = rest.strip_prefix(':')?;
    Some(Prop { name, params, value: value.to_owned(), raw: None })
}

/// A line that couldn't be read is kept as it was, to be written back
/// unchanged.
fn raw_line(line: &str) -> Option<Prop> {
    let end = line.find([';', ':'])?;
    let name = line[..end].trim().to_ascii_uppercase();
    (!name.is_empty() && name != "BEGIN" && name != "END")
        .then(|| Prop { name, params: Vec::new(), value: String::new(), raw: Some(line.to_owned()) })
}

/// Parses an iCalendar object (the outermost component, usually VCALENDAR).
/// Lines that can't be read are skipped rather than failing the whole file.
pub fn parse(text: &str) -> Result<Component> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(cont) = raw.strip_prefix([' ', '\t']) {
            if let Some(last) = lines.last_mut() {
                last.push_str(cont);
            }
        } else if !raw.trim().is_empty() {
            lines.push(raw.to_owned());
        }
    }
    let mut stack: Vec<Component> = Vec::new();
    let mut done: Option<Component> = None;
    for line in &lines {
        let Some(p) = parse_line(line).or_else(|| raw_line(line)) else { continue };
        match p.name.as_str() {
            "BEGIN" => stack.push(Component::new(&p.value.trim().to_ascii_uppercase())),
            "END" => {
                let c = stack.pop().ok_or_else(|| anyhow!("END:{} without BEGIN", p.value))?;
                match stack.last_mut() {
                    Some(parent) => parent.subs.push(c),
                    None => {
                        done = Some(c);
                        break;
                    }
                }
            }
            _ => {
                if let Some(c) = stack.last_mut() {
                    c.props.push(p);
                }
            }
        }
    }
    done.ok_or_else(|| anyhow!("not an iCalendar object"))
}

/// Folds a content line at 75 octets (RFC 5545 §3.1), on character boundaries.
fn fold(line: &str, out: &mut String) {
    let mut width = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if width + n > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += n;
    }
    out.push_str("\r\n");
}

fn quote(v: &str) -> String {
    if v.starts_with('"') {
        v.to_owned()
    } else if v.contains([':', ';', ',']) {
        format!("\"{v}\"")
    } else {
        v.to_owned()
    }
}

pub fn write(c: &Component) -> String {
    let mut out = String::new();
    write_into(c, &mut out);
    out
}

fn write_into(c: &Component, out: &mut String) {
    fold(&format!("BEGIN:{}", c.name), out);
    for p in &c.props {
        if let Some(raw) = &p.raw {
            fold(raw, out);
            continue;
        }
        let mut line = p.name.clone();
        for (k, v) in &p.params {
            line += &format!(";{k}={}", quote(v));
        }
        line.push(':');
        line += &p.value;
        fold(&line, out);
    }
    for s in &c.subs {
        write_into(s, out);
    }
    fold(&format!("END:{}", c.name), out);
}

// ---------------------------------------------------------------------------
// Dates and times

/// A DATE or DATE-TIME value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum When {
    Date(NaiveDate),
    /// No zone: the same wall time everywhere.
    Floating(NaiveDateTime),
    Utc(NaiveDateTime),
    Zoned(NaiveDateTime, Tz),
}

/// A TZID: an IANA name, one with a vendor prefix such as
/// "/mozilla.org/20050126_1/America/New_York", or a Windows zone name (as
/// Outlook and Exchange invitations use). Unknown zones give `None`.
pub fn parse_tzid(s: &str) -> Option<Tz> {
    let s = s.trim().trim_matches('"');
    if let Some(tz) = recurrence::parse_zone(s) {
        return Some(tz);
    }
    if let Some((_, iana)) = WINDOWS_ZONES.iter().find(|(w, _)| w.eq_ignore_ascii_case(s)) {
        return recurrence::parse_zone(iana);
    }
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    (1..parts.len()).find_map(|i| recurrence::parse_zone(&parts[i..].join("/")))
}

/// Windows zone names → IANA (CLDR's "001" mappings), for the zones
/// invitations most often carry.
const WINDOWS_ZONES: &[(&str, &str)] = &[
    ("Dateline Standard Time", "Etc/GMT+12"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Pacific Standard Time (Mexico)", "America/Tijuana"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Mountain Standard Time", "America/Denver"),
    ("Central America Standard Time", "America/Guatemala"),
    ("Central Standard Time", "America/Chicago"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("Canada Central Standard Time", "America/Regina"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("Eastern Standard Time", "America/New_York"),
    ("US Eastern Standard Time", "America/Indiana/Indianapolis"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Venezuela Standard Time", "America/Caracas"),
    ("Pacific SA Standard Time", "America/Santiago"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("UTC", "Etc/UTC"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Arab Standard Time", "Asia/Riyadh"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("E. Africa Standard Time", "Africa/Nairobi"),
    ("Iran Standard Time", "Asia/Tehran"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("Pakistan Standard Time", "Asia/Karachi"),
    ("India Standard Time", "Asia/Calcutta"),
    ("Nepal Standard Time", "Asia/Katmandu"),
    ("Bangladesh Standard Time", "Asia/Dhaka"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Cen. Australia Standard Time", "Australia/Adelaide"),
    ("AUS Central Standard Time", "Australia/Darwin"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("Tasmania Standard Time", "Australia/Hobart"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
];

fn when_value(v: &str, date: bool, tz: Option<Tz>) -> Option<When> {
    let v = v.trim();
    if date || v.len() == 8 {
        return NaiveDate::parse_from_str(v, DATE).ok().map(When::Date);
    }
    if let Some(utc) = v.strip_suffix('Z') {
        return NaiveDateTime::parse_from_str(utc, STAMP).ok().map(When::Utc);
    }
    let t = NaiveDateTime::parse_from_str(v, STAMP).ok()?;
    Some(match tz {
        Some(tz) => When::Zoned(t, tz),
        None => When::Floating(t),
    })
}

/// The values of a date property (EXDATE and RDATE may list several).
pub fn whens(p: &Prop) -> Vec<When> {
    let date = p.param("VALUE") == Some("DATE");
    let tz = p.param("TZID").and_then(parse_tzid);
    p.value.split(',').filter_map(|v| when_value(v, date, tz)).collect()
}

pub fn when(p: &Prop) -> Option<When> {
    whens(p).into_iter().next()
}

/// How a resource's times are read: the zone of the master's DTSTART
/// ("series time"). `None` for floating and all-day events (local wall time).
#[derive(Clone, Debug)]
struct Series {
    zone: Option<Tz>,
    all_day: bool,
    /// The master's DTSTART form, to write RECURRENCE-ID/EXDATE like it.
    form: When,
    /// Its TZID as written (it may be an alias or carry a vendor prefix),
    /// so occurrence stamps name the zone exactly the way the file does.
    tzid: Option<String>,
}

impl Series {
    fn of(dtstart: &Prop) -> Option<Self> {
        let form = when(dtstart)?;
        let (zone, all_day) = match form {
            When::Date(_) => (None, true),
            When::Floating(_) => (None, false),
            When::Utc(_) => (Some(chrono_tz::UTC), false),
            When::Zoned(_, tz) => (Some(tz), false),
        };
        Some(Series { zone, all_day, form, tzid: dtstart.param("TZID").map(str::to_owned) })
    }

    /// A value as series wall time.
    fn wall(&self, w: When) -> NaiveDateTime {
        match (w, self.zone) {
            (When::Date(d), _) => midnight(d),
            (When::Floating(t), _) => t,
            (When::Utc(t), Some(z)) => convert(t, &Utc, &z),
            (When::Utc(t), None) => convert(t, &Utc, &Local),
            (When::Zoned(t, tz), Some(z)) => convert(t, &tz, &z),
            (When::Zoned(t, tz), None) => convert(t, &tz, &Local),
        }
    }

    /// Series wall time as this computer's wall time.
    fn local(&self, t: NaiveDateTime) -> NaiveDateTime {
        recurrence::from_zone(t, self.zone)
    }

    /// A property naming the occurrence at `key_time` (series time), written
    /// the way the master's DTSTART is.
    fn stamp_prop(&self, name: &str, t: NaiveDateTime) -> Prop {
        match self.form {
            When::Date(_) => Prop::new(name, t.format(DATE).to_string()).with("VALUE", "DATE"),
            When::Floating(_) => Prop::new(name, t.format(STAMP).to_string()),
            When::Utc(_) => Prop::new(name, format!("{}Z", t.format(STAMP))),
            When::Zoned(_, tz) => {
                Prop::new(name, t.format(STAMP).to_string()).with("TZID", self.tzid.as_deref().unwrap_or(tz.name()))
            }
        }
    }
}

/// The key of an occurrence: its original start in series time.
pub fn key(t: NaiveDateTime, all_day: bool) -> String {
    if all_day {
        t.format(DATE).to_string()
    } else {
        t.format(STAMP).to_string()
    }
}

fn parse_key(k: &str) -> Option<NaiveDateTime> {
    match k.len() {
        8 => NaiveDate::parse_from_str(k, DATE).ok().map(midnight),
        _ => NaiveDateTime::parse_from_str(k, STAMP).ok(),
    }
}

/// `<href>#<key>` → (`<href>`, `Some(<key>)`).
pub fn split_remote_id(rid: &str) -> (&str, Option<&str>) {
    match rid.split_once('#') {
        Some((href, key)) => (href, Some(key)),
        None => (rid, None),
    }
}

/// A DURATION value ("PT1H30M", "P2D", "-PT15M").
fn parse_duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    let (neg, v) = match v.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut num = String::new();
    let mut in_time = false;
    for c in v.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            _ => {
                // Out-of-range values (from a broken or hostile server)
                // make the duration invalid rather than panicking.
                let n: i64 = std::mem::take(&mut num).parse().ok()?;
                let part = match (c, in_time) {
                    ('W', _) => Duration::try_weeks(n),
                    ('D', _) => Duration::try_days(n),
                    ('H', true) => Duration::try_hours(n),
                    ('M', true) => Duration::try_minutes(n),
                    ('S', true) => Duration::try_seconds(n),
                    _ => return None,
                }?;
                total = total.checked_add(&part)?;
            }
        }
    }
    Some(if neg { -total } else { total })
}

// ---------------------------------------------------------------------------
// Reading events

/// One VEVENT's own times, as series wall time: (start, end).
fn span(ev: &Component, s: &Series) -> Option<(NaiveDateTime, NaiveDateTime)> {
    let start_w = when(ev.prop("DTSTART")?)?;
    let start = s.wall(start_w);
    let end = match (ev.prop("DTEND").and_then(when), ev.prop("DURATION").and_then(|p| parse_duration(&p.value))) {
        (Some(e), _) => s.wall(e),
        (None, Some(d)) => start.checked_add_signed(d).unwrap_or(start),
        // RFC 5545: a day for a date, nothing for a date-time.
        (None, None) if matches!(start_w, When::Date(_)) => start + Duration::days(1),
        (None, None) => start,
    };
    Some((start, end.max(start)))
}

/// The app's event for one VEVENT.
#[allow(clippy::too_many_arguments)]
fn to_event(
    cal: &CalRef,
    vevent: &Component,
    remote_id: String,
    recurring_id: Option<String>,
    etag: Option<&str>,
    s: &Series,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Event {
    // Its own DTSTART decides all-day and zone (an override may differ).
    let own = vevent.prop("DTSTART").and_then(when);
    let all_day = match own {
        Some(When::Date(_)) => true,
        Some(_) => false,
        None => s.all_day,
    };
    let time_zone = match own {
        Some(When::Zoned(_, tz)) if !all_day => Some(tz.name().to_owned()),
        _ => None,
    };
    let title = vevent.text("SUMMARY").filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "(No title)".into());
    let (start, end) = if all_day { (midnight(start.date()), midnight(end.date()).max(midnight(start.date()) + Duration::days(1))) } else { (s.local(start), s.local(end)) };
    Event {
        id: cal.event_id(&remote_id),
        calendar_id: cal.local_id(),
        title,
        start,
        end,
        all_day,
        location: vevent.text("LOCATION").unwrap_or_default(),
        notes: vevent.text("DESCRIPTION").unwrap_or_default(),
        // Invitations go through Google only; a CalDAV event's ATTENDEEs
        // stay in its .ics (saving keeps them) but aren't edited here.
        attendees: Vec::new(),
        remote_id: Some(remote_id),
        recurring_id,
        pending: false,
        rev: 0,
        etag: etag.map(str::to_owned),
        base: None,
        recurrence: None,
        time_zone,
    }
}

fn is_cancelled(v: &Component) -> bool {
    v.prop("STATUS").is_some_and(|p| p.value.trim().eq_ignore_ascii_case("CANCELLED"))
}

/// The master VEVENT, if any.
fn master(cal: &Component) -> Option<&Component> {
    cal.events().find(|e| e.prop("RECURRENCE-ID").is_none())
}

fn repeats(master: &Component) -> bool {
    master.prop("RRULE").is_some() || master.prop("RDATE").is_some()
}

/// Occurrence starts (series time) of a repeating master in [from, to].
fn expand(master: &Component, s: &Series, from: NaiveDateTime, to: NaiveDateTime) -> Vec<NaiveDateTime> {
    let Some((first, _)) = span(master, s) else { return Vec::new() };
    let mut out: Vec<NaiveDateTime> = Vec::new();
    let mut until: Option<NaiveDateTime> = None;
    if let Some(rule) = master.prop("RRULE") {
        // UNTIL is applied here, in series time: the rrule crate is strict
        // about how its zone must match DTSTART's, servers aren't.
        let mut parts = Vec::new();
        for part in rule.value.split(';').filter(|p| !p.is_empty()) {
            match part.split_once('=') {
                Some((k, v)) if k.eq_ignore_ascii_case("UNTIL") => {
                    until = when_value(v, false, s.zone).map(|w| match w {
                        When::Date(d) => d.and_hms_opt(23, 59, 59).unwrap(),
                        w => s.wall(w),
                    });
                }
                _ => parts.push(part),
            }
        }
        // Expanded in the series zone (floating and all-day: UTC as a
        // stand-in for "wall time").
        let tz = rrule::Tz::Tz(s.zone.unwrap_or(chrono_tz::UTC));
        let at = |t: NaiveDateTime| tz.from_local_datetime(&t).earliest().or_else(|| tz.from_local_datetime(&(t + Duration::hours(1))).earliest());
        if let (Some(start), Some(lo), Some(hi)) = (at(first), at(from), at(to)) {
            let rule: Option<rrule::RRule> =
                parts.join(";").parse::<rrule::RRule<rrule::Unvalidated>>().ok().and_then(|r| r.validate(start).ok());
            if let Some(rule) = rule {
                let set = rrule::RRuleSet::new(start).rrule(rule).after(lo).before(hi);
                out.extend(set.all(5000).dates.into_iter().map(|d| d.naive_local()));
            }
        }
    }
    for p in master.props_named("RDATE") {
        out.extend(whens(p).into_iter().map(|w| s.wall(w)));
    }
    // The first occurrence always counts (RFC 5545 §3.8.5.3).
    out.push(first);
    let excluded: Vec<NaiveDateTime> = master.props_named("EXDATE").flat_map(|p| whens(p).into_iter().map(|w| s.wall(w))).collect();
    out.retain(|t| *t >= from && *t <= to && until.map_or(true, |u| *t <= u) && !excluded.contains(t));
    out.sort();
    out.dedup();
    out
}

/// Every event in one resource that overlaps [from, to) (local dates), as
/// the app shows them.
pub fn events(cal: &CalRef, href: &str, etag: Option<&str>, ics: &Component, from: NaiveDate, to: NaiveDate) -> Vec<Event> {
    let master_ev = master(ics);
    let Some(series) = master_ev.or_else(|| ics.events().next()).and_then(|m| m.prop("DTSTART")).and_then(Series::of)
    else {
        return Vec::new();
    };
    let s = &series;
    // Plain event.
    if let Some(m) = master_ev.filter(|m| !repeats(m)) {
        let Some((start, end)) = span(m, s) else { return Vec::new() };
        return vec![to_event(cal, m, href.to_owned(), None, etag, s, start, end)];
    }
    let (lo, hi) = (midnight(from) - Duration::days(1), midnight(to) + Duration::days(1));
    let mut out = Vec::new();
    let mut overridden: Vec<NaiveDateTime> = Vec::new();
    for o in ics.events().filter(|e| e.prop("RECURRENCE-ID").is_some()) {
        let Some(rid) = o.prop("RECURRENCE-ID").and_then(when).map(|w| s.wall(w)) else { continue };
        overridden.push(rid);
        let Some((start, end)) = span(o, s) else { continue };
        if is_cancelled(o) || end < lo || start > hi {
            continue;
        }
        let remote_id = format!("{href}#{}", key(rid, s.all_day));
        out.push(to_event(cal, o, remote_id, Some(href.to_owned()), etag, s, start, end));
    }
    if let Some(m) = master_ev {
        let Some((first, first_end)) = span(m, s) else { return out };
        let len = first_end - first;
        for t in expand(m, s, lo - len, hi) {
            if overridden.contains(&t) {
                continue;
            }
            let remote_id = format!("{href}#{}", key(t, s.all_day));
            out.push(to_event(cal, m, remote_id, Some(href.to_owned()), etag, s, t, t + len));
        }
    }
    out.sort_by_key(|e| e.start);
    out
}

/// The event with this remote id in a resource (`None`: not there, e.g. a
/// deleted occurrence).
pub fn find(cal: &CalRef, rid: &str, etag: Option<&str>, ics: &Component) -> Option<Event> {
    let (href, key_part) = split_remote_id(rid);
    let master_ev = master(ics);
    let s = master_ev.or_else(|| ics.events().next()).and_then(|m| m.prop("DTSTART")).and_then(Series::of)?;
    let Some(k) = key_part else {
        let m = master_ev.filter(|m| !repeats(m))?;
        let (start, end) = span(m, &s)?;
        return Some(to_event(cal, m, href.to_owned(), None, etag, &s, start, end));
    };
    let t = parse_key(k)?;
    for o in ics.events() {
        let Some(r) = o.prop("RECURRENCE-ID").and_then(when).map(|w| s.wall(w)) else { continue };
        if r == t {
            if is_cancelled(o) {
                return None;
            }
            let (start, end) = span(o, &s)?;
            return Some(to_event(cal, o, rid.to_owned(), Some(href.to_owned()), etag, &s, start, end));
        }
    }
    let m = master_ev?;
    let (first, first_end) = span(m, &s)?;
    expand(m, &s, t, t).contains(&t).then(|| {
        let len = first_end - first;
        to_event(cal, m, rid.to_owned(), Some(href.to_owned()), etag, &s, t, t + len)
    })
}

/// Whether the occurrence with this key has been changed on its own (has
/// an override).
pub fn has_override(cal: &Component, key: &str) -> bool {
    let (Some(t), Some(s)) = (parse_key(key), master(cal).and_then(|m| m.prop("DTSTART")).and_then(Series::of)) else {
        return false;
    };
    cal.events().any(|e| e.prop("RECURRENCE-ID").and_then(when).is_some_and(|w| s.wall(w) == t))
}

/// The resource's UID (of its first VEVENT).
pub fn uid(ics: &Component) -> Option<String> {
    ics.events().find_map(|e| e.text("UID"))
}

// ---------------------------------------------------------------------------
// Writing events

fn now_utc() -> String {
    format!("{}Z", Utc::now().naive_utc().format(STAMP))
}

/// A start or end property for `t` (local wall time).
fn time_prop(name: &str, t: NaiveDateTime, ev: &Event, zone: Option<Tz>) -> Prop {
    if ev.all_day {
        return Prop::new(name, t.format(DATE).to_string()).with("VALUE", "DATE");
    }
    match zone {
        Some(tz) => Prop::new(name, recurrence::to_zone(t, Some(tz)).format(STAMP).to_string()).with("TZID", tz.name()),
        None => {
            // A time skipped by a daylight-saving change counts as the hour after.
            let at = |t: NaiveDateTime| Local.from_local_datetime(&t).earliest();
            let utc: DateTime<Utc> = at(t)
                .or_else(|| at(t + Duration::hours(1)))
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|| t.and_utc());
            Prop::new(name, format!("{}Z", utc.naive_utc().format(STAMP)))
        }
    }
}

/// Writes the app-owned fields of `ev` into a VEVENT, keeping the rest.
/// `master_series`: writing a master, whose repeat rule `ev` carries.
fn fill(v: &mut Component, ev: &Event, master_series: bool) {
    // A pinned zone, else (for a repeating event, which needs one to repeat
    // in) this computer's zone, else UTC.
    let zone = match (&ev.time_zone, &ev.recurrence) {
        _ if ev.all_day => None,
        (Some(z), _) => recurrence::parse_zone(z),
        (None, Some(_)) if master_series => recurrence::local_zone_name().and_then(|z| recurrence::parse_zone(&z)),
        _ => None,
    };
    v.set_text("SUMMARY", &ev.title);
    // Apple's map pin for the location would still show the old place.
    if v.text("LOCATION").unwrap_or_default() != ev.location {
        v.remove("X-APPLE-STRUCTURED-LOCATION");
    }
    v.set_text("LOCATION", &ev.location);
    v.set_text("DESCRIPTION", &ev.notes);
    v.set(time_prop("DTSTART", ev.start, ev, zone));
    v.remove("DURATION");
    v.set(time_prop("DTEND", ev.end, ev, zone));
    v.set(Prop::new("DTSTAMP", now_utc()));
    v.set(Prop::new("LAST-MODIFIED", now_utc()));
    let seq = v.prop("SEQUENCE").and_then(|p| p.value.trim().parse::<u32>().ok());
    v.set(Prop::new("SEQUENCE", seq.map_or(0, |n| n.saturating_add(1)).to_string()));
    if master_series {
        for name in ["RRULE", "RDATE", "EXDATE", "EXRULE"] {
            v.remove(name);
        }
        if let Some(rule) = &ev.recurrence {
            let zname = zone.map(|z| z.name().to_owned()).unwrap_or_else(|| "UTC".into());
            for line in recurrence::to_rrule(rule, ev, &zname) {
                if let Some(p) = parse_line(&line) {
                    v.props.push(p);
                }
            }
        }
    }
}

fn new_calendar() -> Component {
    let mut c = Component::new("VCALENDAR");
    c.props.push(Prop::new("VERSION", "2.0"));
    c.props.push(Prop::new("PRODID", PRODID));
    c.props.push(Prop::new("CALSCALE", "GREGORIAN"));
    c
}

/// Writes `ev` into a resource: a new one (`existing` is `None`, with this
/// UID), the whole event or series (`key` is `None`), or one occurrence of a
/// series (as an override).
pub fn put(existing: Option<Component>, ev: &Event, uid: &str, key: Option<&str>) -> Result<Component> {
    let mut cal = existing.unwrap_or_else(new_calendar);
    match key {
        None => {
            let i = match cal.subs.iter().position(|c| c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_none()) {
                Some(i) => i,
                None => {
                    let mut v = Component::new("VEVENT");
                    v.props.push(Prop::new("UID", escape_text(uid)));
                    v.props.push(Prop::new("CREATED", now_utc()));
                    cal.subs.push(v);
                    cal.subs.len() - 1
                }
            };
            fill(&mut cal.subs[i], ev, true);
        }
        Some(k) => {
            let t = parse_key(k).ok_or_else(|| anyhow!("bad occurrence key {k}"))?;
            let m = master(&cal).ok_or_else(|| anyhow!("the series is gone"))?.clone();
            let s = m.prop("DTSTART").and_then(Series::of).ok_or_else(|| anyhow!("series without a start"))?;
            let existing = cal.subs.iter().position(|c| {
                c.name == "VEVENT" && c.prop("RECURRENCE-ID").and_then(when).is_some_and(|w| s.wall(w) == t)
            });
            let i = match existing {
                Some(i) => i,
                None => {
                    // A new override: the series' details, for this occurrence.
                    let mut v = m.clone();
                    for name in ["RRULE", "RDATE", "EXDATE", "EXRULE", "DTSTART", "DTEND", "DURATION"] {
                        v.remove(name);
                    }
                    v.props.push(s.stamp_prop("RECURRENCE-ID", t));
                    v.subs.retain(|c| c.name == "VALARM");
                    cal.subs.push(v);
                    cal.subs.len() - 1
                }
            };
            fill(&mut cal.subs[i], ev, false);
        }
    }
    add_timezones(&mut cal);
    Ok(cal)
}

/// Removes one occurrence from a series (an EXDATE, and its override if it
/// had one). Returns `false` when nothing of the event is left.
pub fn remove_occurrence(cal: &mut Component, key: &str) -> Result<bool> {
    let t = parse_key(key).ok_or_else(|| anyhow!("bad occurrence key {key}"))?;
    let s = cal
        .events()
        .find(|e| e.prop("RECURRENCE-ID").is_none())
        .or_else(|| cal.events().next())
        .and_then(|m| m.prop("DTSTART"))
        .and_then(Series::of)
        .ok_or_else(|| anyhow!("no event in the resource"))?;
    cal.subs.retain(|c| !(c.name == "VEVENT" && c.prop("RECURRENCE-ID").and_then(when).is_some_and(|w| s.wall(w) == t)));
    if let Some(m) = cal.subs.iter_mut().find(|c| c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_none()) {
        m.props.push(s.stamp_prop("EXDATE", t));
        let seq = m.prop("SEQUENCE").and_then(|p| p.value.trim().parse::<u32>().ok());
        m.set(Prop::new("SEQUENCE", seq.map_or(0, |n| n.saturating_add(1)).to_string()));
        m.set(Prop::new("DTSTAMP", now_utc()));
    }
    add_timezones(cal);
    Ok(cal.events().next().is_some())
}

/// Adds a VTIMEZONE for every TZID used that doesn't have one (RFC 5545
/// requires them), built from the zone database.
fn add_timezones(cal: &mut Component) {
    let mut used: Vec<String> = Vec::new();
    for e in cal.events() {
        for p in &e.props {
            if let Some(z) = p.param("TZID") {
                if !used.iter().any(|u| u == z) {
                    used.push(z.to_owned());
                }
            }
        }
    }
    let have: Vec<String> =
        cal.subs.iter().filter(|c| c.name == "VTIMEZONE").filter_map(|c| c.prop("TZID").map(|p| p.value.clone())).collect();
    let year = cal
        .events()
        .filter_map(|e| e.prop("DTSTART").and_then(when))
        .map(|w| match w {
            When::Date(d) => d,
            When::Floating(t) | When::Utc(t) | When::Zoned(t, _) => t.date(),
        })
        .min()
        .map_or(2026, |d| chrono::Datelike::year(&d));
    let mut zones: Vec<Component> =
        used.iter().filter(|z| !have.contains(z)).filter_map(|z| Some(vtimezone(parse_tzid(z)?, z, year))).collect();
    // VTIMEZONEs go before the events that use them.
    let first_event = cal.subs.iter().position(|c| c.name == "VEVENT").unwrap_or(cal.subs.len());
    for (i, z) in zones.drain(..).enumerate() {
        cal.subs.insert(first_event + i, z);
    }
}

/// A VTIMEZONE listing the zone's offset changes from the year before
/// `year` for 20 years.
pub fn vtimezone(tz: Tz, tzid: &str, year: i32) -> Component {
    let mut c = Component::new("VTIMEZONE");
    c.props.push(Prop::new("TZID", tzid));
    let offset_at = |t: NaiveDateTime| {
        let o = tz.offset_from_utc_datetime(&t);
        (chrono::Offset::fix(&o).local_minus_utc(), !o.dst_offset().is_zero(), o.abbreviation().map(str::to_owned))
    };
    let fmt = |secs: i32| {
        let sign = if secs < 0 { '-' } else { '+' };
        let a = secs.abs();
        format!("{sign}{:02}{:02}", a / 3600, (a % 3600) / 60)
    };
    let start = NaiveDate::from_ymd_opt(year - 1, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let end = NaiveDate::from_ymd_opt(year + 20, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let mut t = start;
    let mut prev = offset_at(t);
    let mut changes = 0;
    while t < end {
        let next = t + Duration::days(1);
        let cur = offset_at(next);
        if cur.0 != prev.0 || cur.1 != prev.1 {
            // Find the second it changed.
            let (mut lo, mut hi) = (t, next);
            while hi - lo > Duration::seconds(1) {
                let mid = lo + (hi - lo) / 2;
                if offset_at(mid).0 == prev.0 && offset_at(mid).1 == prev.1 {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let mut sub = Component::new(if cur.1 { "DAYLIGHT" } else { "STANDARD" });
            // DTSTART is in the local time before the change.
            sub.props.push(Prop::new("DTSTART", (hi + Duration::seconds(prev.0 as i64)).format(STAMP).to_string()));
            sub.props.push(Prop::new("TZOFFSETFROM", fmt(prev.0)));
            sub.props.push(Prop::new("TZOFFSETTO", fmt(cur.0)));
            if let Some(name) = &cur.2 {
                sub.props.push(Prop::new("TZNAME", name.clone()));
            }
            c.subs.push(sub);
            changes += 1;
            prev = cur;
        }
        t = next;
    }
    if changes == 0 {
        let mut sub = Component::new("STANDARD");
        sub.props.push(Prop::new("DTSTART", "19700101T000000"));
        sub.props.push(Prop::new("TZOFFSETFROM", fmt(prev.0)));
        sub.props.push(Prop::new("TZOFFSETTO", fmt(prev.0)));
        if let Some(name) = &prev.2 {
            sub.props.push(Prop::new("TZNAME", name.clone()));
        }
        c.subs.push(sub);
    }
    c
}

#[cfg(test)]
mod tests;
