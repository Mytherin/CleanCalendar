//! Core data model: calendars, events, and the in-memory store (saved by `db`).

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::recurrence::{self, Recurrence};
use crate::remote::{CalRef, Login, GOOGLE};
use crate::settings::Settings;
use crate::{caldav, google};

/// Apple's system palette, used for new calendars.
pub const CALENDAR_COLORS: [[u8; 3]; 7] = [
    [0xFF, 0x3B, 0x30], // red
    [0xFF, 0x95, 0x00], // orange
    [0xFF, 0xCC, 0x00], // yellow
    [0x34, 0xC7, 0x59], // green
    [0x00, 0x7A, 0xFF], // blue
    [0xAF, 0x52, 0xDE], // purple
    [0xA2, 0x84, 0x5E], // brown
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    Local,
    Google { calendar_id: String },
    /// A calendar collection on a CalDAV server (iCloud and others).
    CalDav { account: String, url: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    pub visible: bool,
    pub source: Source,
    #[serde(default)]
    pub read_only: bool,
}

impl Calendar {
    /// Where the calendar lives, if it's synced with a service.
    pub fn remote(&self) -> Option<CalRef> {
        match &self.source {
            Source::Local => None,
            Source::Google { calendar_id } => Some(CalRef::google(calendar_id)),
            Source::CalDav { account, url } => Some(CalRef::new(account, url)),
        }
    }

    /// Google's birthdays calendar (from your contacts).
    pub fn is_birthdays(&self) -> bool {
        self.google_id().is_some_and(|id| id.contains("#contacts@") || id.contains("#birthdays@"))
    }

    /// One of Google's public holiday calendars.
    pub fn is_holidays(&self) -> bool {
        self.google_id().is_some_and(|id| id.contains("#holiday@"))
    }

    /// The Google calendar id, for a calendar on the Google account.
    pub fn google_id(&self) -> Option<&str> {
        match &self.source {
            Source::Google { calendar_id } => Some(calendar_id),
            _ => None,
        }
    }

    /// The account it's synced with.
    pub fn account(&self) -> Option<&str> {
        match &self.source {
            Source::Local => None,
            Source::Google { .. } => Some(GOOGLE),
            Source::CalDav { account, .. } => Some(account),
        }
    }

    pub fn is_remote(&self) -> bool {
        !matches!(self.source, Source::Local)
    }
}

/// An event. Times are naive local wall-clock times (see [`crate::recurrence`]
/// for events pinned to a time zone). For all-day events `start` is midnight
/// of the first day and `end` is midnight of the day *after* the last day
/// (exclusive), matching Google's convention.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub all_day: bool,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub notes: String,
    /// People invited to the event (empty for an event without guests).
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    /// The event's id on the remote service, if it lives on one.
    #[serde(default)]
    pub remote_id: Option<String>,
    /// For one occurrence of a recurring remote event: the series id.
    #[serde(default)]
    pub recurring_id: Option<String>,
    /// Local changes that the remote service hasn't confirmed yet.
    #[serde(default)]
    pub pending: bool,
    /// Bumped on every local change, so a late confirmation of an older
    /// upload doesn't clear `pending` for a newer edit.
    #[serde(default)]
    pub rev: u64,
    /// The remote version tag of the copy we last saw. Uploads send it so
    /// the server refuses them if someone else changed the event meanwhile.
    #[serde(default)]
    pub etag: Option<String>,
    /// While local edits are pending: the event as the server last had it.
    /// With it, a conflict can be merged field by field (three-way merge).
    #[serde(default)]
    pub base: Option<EventFields>,
    /// How the event repeats; `start`/`end` are then the first occurrence.
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    /// The IANA zone a timed event is pinned to (e.g. "America/New_York");
    /// `None`: floating, it happens at the same wall time wherever you are.
    #[serde(default)]
    pub time_zone: Option<String>,
}

/// The user-editable parts of an event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventFields {
    pub title: String,
    pub location: String,
    pub notes: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub all_day: bool,
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
}

/// How an invitee answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    #[default]
    NeedsAction,
    Accepted,
    Tentative,
    Declined,
}

impl Response {
    /// Google's `responseStatus` value.
    pub fn as_google(self) -> &'static str {
        match self {
            Response::NeedsAction => "needsAction",
            Response::Accepted => "accepted",
            Response::Tentative => "tentative",
            Response::Declined => "declined",
        }
    }

    pub fn from_google(s: &str) -> Self {
        match s {
            "accepted" => Response::Accepted,
            "tentative" => Response::Tentative,
            "declined" => Response::Declined,
            _ => Response::NeedsAction,
        }
    }
}

/// Someone invited to an event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    /// Display name, if known.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub response: Response,
    #[serde(default)]
    pub organizer: bool,
    /// This is the signed-in user.
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub optional: bool,
    /// A room or other resource rather than a person. Kept so that saving
    /// the guest list doesn't drop it, but not shown.
    #[serde(default)]
    pub resource: bool,
}

impl Attendee {
    pub fn new(email: &str) -> Self {
        Attendee {
            email: email.trim().to_owned(),
            name: String::new(),
            response: Response::NeedsAction,
            organizer: false,
            is_self: false,
            optional: false,
            resource: false,
        }
    }

    /// The name if there is one, else the email address.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.email
        } else {
            &self.name
        }
    }
}

/// A loose check that `s` looks like one email address ("a@b.c").
pub fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    let Some((user, domain)) = s.split_once('@') else { return false };
    !user.is_empty()
        && !s.chars().any(|c| c.is_whitespace() || c == ',' || c == ';')
        && !domain.contains('@')
        && domain.split('.').count() >= 2
        && domain.split('.').all(|part| !part.is_empty())
}

impl Event {
    pub fn fields(&self) -> EventFields {
        EventFields {
            title: self.title.clone(),
            location: self.location.clone(),
            notes: self.notes.clone(),
            start: self.start,
            end: self.end,
            all_day: self.all_day,
            recurrence: self.recurrence.clone(),
            time_zone: self.time_zone.clone(),
            attendees: self.attendees.clone(),
        }
    }

    pub fn set_fields(&mut self, f: EventFields) {
        self.title = f.title;
        self.location = f.location;
        self.notes = f.notes;
        self.start = f.start;
        self.end = f.end;
        self.all_day = f.all_day;
        self.recurrence = f.recurrence;
        self.time_zone = f.time_zone;
        self.attendees = f.attendees;
    }

    /// Whether this is (an occurrence of) a repeating event.
    pub fn repeats(&self) -> bool {
        self.recurrence.is_some() || self.recurring_id.is_some()
    }

    /// The id it's shown under: a series shows as its occurrences, so its
    /// first one.
    pub fn shown_id(&self) -> String {
        match self.recurrence {
            Some(_) => recurrence::occurrence_id(&self.id, recurrence::series_start(self)),
            None => self.id.clone(),
        }
    }

    pub fn occurs_on(&self, day: NaiveDate) -> bool {
        let ds = day.and_hms_opt(0, 0, 0).unwrap();
        let de = ds + Duration::days(1);
        if self.end <= self.start {
            return self.start >= ds && self.start < de;
        }
        self.start < de && self.end > ds
    }

    /// Whether this event should be drawn in the all-day lane / as a bar.
    pub fn is_banner(&self) -> bool {
        self.all_day || self.end - self.start >= Duration::hours(24)
    }

    pub fn duration(&self) -> Duration {
        self.end - self.start
    }

    /// A copy of this event as a new, unsynced, non-repeating event on
    /// `calendar_id` starting at `start` (pasting and duplicating).
    pub fn copy_as_new(&self, calendar_id: String, start: NaiveDateTime) -> Event {
        Event {
            id: format!("local:{}", uuid::Uuid::new_v4().simple()),
            calendar_id,
            start,
            end: start + self.duration(),
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
            recurrence: None,
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GoogleSettings {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: Option<String>,
    pub account_email: Option<String>,
}

impl GoogleSettings {
    pub fn is_connected(&self) -> bool {
        self.refresh_token.is_some()
    }
}

/// A CalDAV account (iCloud, Fastmail, Nextcloud, …).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalDavAccount {
    /// The account key: "caldav-<hex>".
    pub id: String,
    /// What the sidebar calls it ("iCloud").
    pub label: String,
    /// Where discovery starts, e.g. "https://caldav.icloud.com/".
    pub server: String,
    pub username: String,
    /// Kept in the OS credential store, not with the rest of the data.
    #[serde(skip)]
    pub password: Option<String>,
}

impl CalDavAccount {
    pub fn new_id() -> String {
        format!("caldav-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
    }

    pub fn login(&self) -> Option<caldav::Login> {
        Some(caldav::Login {
            server: self.server.clone(),
            username: self.username.clone(),
            password: self.password.clone()?,
        })
    }
}

/// A remote deletion that hasn't been confirmed yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "PendingDeleteJson")]
pub struct PendingDelete {
    pub calendar: CalRef,
    pub remote_id: String,
    /// Delete only if the server still has this version (`None`: always).
    pub etag: Option<String>,
}

/// How the old JSON store wrote a [`PendingDelete`] (Google only).
#[derive(Deserialize)]
struct PendingDeleteJson {
    google_calendar_id: String,
    remote_id: String,
    #[serde(default)]
    etag: Option<String>,
}

impl From<PendingDeleteJson> for PendingDelete {
    fn from(j: PendingDeleteJson) -> Self {
        PendingDelete { calendar: CalRef::google(&j.google_calendar_id), remote_id: j.remote_id, etag: j.etag }
    }
}

/// Per-calendar incremental sync bookmark.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    /// The service's sync token; `None` forces a full sync.
    pub token: Option<String>,
    /// When the last full sync happened.
    pub full_sync_on: Option<NaiveDate>,
}

/// Everything the app shows and syncs. `Deserialize` is only for importing
/// the old `store.json`; saving is done by [`crate::db`].
#[derive(Debug, Default, Deserialize)]
pub struct Store {
    pub calendars: Vec<Calendar>,
    pub events: Vec<Event>,
    #[serde(default)]
    pub google: GoogleSettings,
    #[serde(default)]
    pub caldav: Vec<CalDavAccount>,
    #[serde(default)]
    pub pending_deletes: Vec<PendingDelete>,
    /// Keyed by the app's calendar id.
    #[serde(default)]
    pub sync_state: HashMap<String, SyncState>,
    #[serde(default)]
    pub settings: Settings,
    /// Where this store is saved; `None` for one that isn't (tests).
    #[serde(skip)]
    pub(crate) db: Option<crate::db::Db>,
}

impl Store {
    /// Loads from the default location (see [`crate::db::Paths`]).
    pub fn load() -> Self {
        crate::db::open(&crate::db::Paths::default_location())
    }

    pub fn with_defaults() -> Self {
        Store {
            calendars: vec![
                Calendar {
                    id: "local:home".into(),
                    name: "Home".into(),
                    color: CALENDAR_COLORS[4],
                    visible: true,
                    source: Source::Local,
                    read_only: false,
                },
                Calendar {
                    id: "local:work".into(),
                    name: "Work".into(),
                    color: CALENDAR_COLORS[1],
                    visible: true,
                    source: Source::Local,
                    read_only: false,
                },
            ],
            events: Vec::new(),
            google: GoogleSettings::default(),
            caldav: Vec::new(),
            pending_deletes: Vec::new(),
            sync_state: HashMap::new(),
            settings: Settings::default(),
            db: None,
        }
    }

    /// Writes what changed since the last save. Errors are logged and the
    /// next save tries again.
    pub fn save(&mut self) {
        let Some(mut db) = self.db.take() else { return };
        if let Err(e) = db.save(self) {
            eprintln!("Could not save: {e:#}");
        }
        self.db = Some(db);
    }

    pub fn calendar(&self, id: &str) -> Option<&Calendar> {
        self.calendars.iter().find(|c| c.id == id)
    }

    pub fn event(&self, id: &str) -> Option<&Event> {
        self.events.iter().find(|e| e.id == id)
    }

    pub fn event_mut(&mut self, id: &str) -> Option<&mut Event> {
        self.events.iter_mut().find(|e| e.id == id)
    }

    /// An event as shown: a stored event, or an occurrence of a repeating one
    /// (see [`recurrence::occurrence_id`]).
    pub fn find_event(&self, id: &str) -> Option<Event> {
        if let Some(ev) = self.event(id) {
            return Some(ev.clone());
        }
        let (series, _) = recurrence::split_occurrence(id)?;
        recurrence::occurrence(self.event(series)?, id)
    }

    /// The next (or previous) occurrence of the repeating event that the
    /// shown event `id` is part of. It looks at a series stored here (expanded
    /// into its occurrences) as well as stored events of the same series
    /// (occurrences Google sent one by one, or ones changed on their own).
    pub fn adjacent_occurrence(&self, id: &str, forward: bool) -> Option<Event> {
        /// How far ahead to look for the next occurrence.
        const HORIZON_DAYS: i64 = 6 * 366;
        let ev = self.find_event(id)?;
        let series_id = match recurrence::split_occurrence(id) {
            Some((series, _)) if self.event(id).is_none() => series.to_owned(),
            _ if ev.recurrence.is_some() => ev.id.clone(),
            _ => ev.recurring_id.clone()?,
        };
        let mut found: Vec<Event> = self
            .events
            .iter()
            .filter(|e| e.id != ev.id && e.recurrence.is_none() && e.calendar_id == ev.calendar_id)
            .filter(|e| e.recurring_id.as_deref() == Some(series_id.as_str()))
            .cloned()
            .collect();
        if let Some(series) = self.event(&series_id).filter(|s| s.recurrence.is_some()) {
            let (from, to) = if forward {
                (ev.start, ev.start + Duration::days(HORIZON_DAYS))
            } else {
                (series.start.min(ev.start) - Duration::days(1), ev.start)
            };
            found.extend(recurrence::occurrences(series, from, to));
        }
        let found = found.into_iter().filter(|e| e.id != id);
        if forward {
            found.filter(|e| e.start > ev.start).min_by_key(|e| e.start)
        } else {
            found.filter(|e| e.start < ev.start).max_by_key(|e| e.start)
        }
    }

    /// The remote calendar an event lives on, if any.
    pub fn remote_of(&self, ev: &Event) -> Option<CalRef> {
        self.calendar(&ev.calendar_id).and_then(Calendar::remote)
    }

    /// The signed-in accounts, Google first.
    pub fn accounts(&self) -> Vec<String> {
        // A Google account switched off in Settings isn't synced.
        let google = (self.google.is_connected() && self.settings.google_enabled).then(|| GOOGLE.to_owned());
        google.into_iter().chain(self.caldav.iter().filter(|a| a.password.is_some()).map(|a| a.id.clone())).collect()
    }

    pub fn caldav_account(&self, id: &str) -> Option<&CalDavAccount> {
        self.caldav.iter().find(|a| a.id == id)
    }

    /// How to sign in to an account; `None` when signed out.
    pub fn login(&self, account: &str) -> Option<Login> {
        if account == GOOGLE {
            let g = &self.google;
            return Some(Login::Google(google::Creds {
                client_id: g.client_id.clone(),
                client_secret: g.client_secret.clone(),
                refresh_token: g.refresh_token.clone()?,
            }));
        }
        self.caldav_account(account)?.login().map(Login::CalDav)
    }

    /// What to call an account in messages and the sidebar.
    pub fn account_name(&self, account: &str) -> String {
        if account == GOOGLE {
            return "Google".into();
        }
        self.caldav_account(account).map_or_else(|| "the server".into(), |a| a.label.clone())
    }

    /// Number of local changes not yet confirmed by a service.
    pub fn unsynced_count(&self) -> usize {
        self.events.iter().filter(|e| e.pending).count() + self.pending_deletes.len()
    }

    /// Whether a calendar is listed at all (in the sidebar, the editor's
    /// picker, the views): not if its account is switched off, or it is a
    /// birthdays/holidays calendar the user chose not to show.
    pub fn is_listed(&self, c: &Calendar) -> bool {
        let s = &self.settings;
        match c.source {
            Source::Local => true,
            Source::Google { .. } => {
                s.google_enabled && (s.show_birthdays || !c.is_birthdays()) && (s.show_holidays || !c.is_holidays())
            }
            Source::CalDav { .. } => true,
        }
    }

    pub fn is_visible(&self, calendar_id: &str) -> bool {
        self.calendar(calendar_id).is_some_and(|c| c.visible && self.is_listed(c))
    }

    pub fn writable_calendars(&self) -> impl Iterator<Item = &Calendar> {
        self.calendars.iter().filter(|c| !c.read_only && self.is_listed(c))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::recurrence::{Freq, Recurrence};

    fn t(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn event_at(id: &str, start: &str, end: &str) -> Event {
        Event {
            id: id.into(),
            calendar_id: "local:home".into(),
            title: id.into(),
            start: t(start),
            end: t(end),
            all_day: false,
            location: "Room 1".into(),
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

    fn store(events: Vec<Event>) -> Store {
        Store { events, ..Store::with_defaults() }
    }

    #[test]
    fn steps_through_a_series_stored_here() {
        let series = Event { recurrence: Some(Recurrence::new(Freq::Weekly)), ..event_at("s", "2026-11-02 12:00", "2026-11-02 13:00") };
        let s = store(vec![series]);
        let second = recurrence::occurrence_id("s", t("2026-11-09 12:00"));
        let next = s.adjacent_occurrence(&second, true).unwrap();
        assert_eq!(next.start, t("2026-11-16 12:00"));
        assert_eq!(next.id, recurrence::occurrence_id("s", t("2026-11-16 12:00")));
        let prev = s.adjacent_occurrence(&second, false).unwrap();
        assert_eq!(prev.start, t("2026-11-02 12:00"));
        assert!(s.adjacent_occurrence(&prev.id, false).is_none(), "nothing before the first");
    }

    #[test]
    fn skips_removed_occurrences_and_finds_detached_ones() {
        // Daily at 09:00; the 3rd was deleted and the 4th moved to 15:00 on its own.
        let mut rule = Recurrence::new(Freq::Daily);
        rule.exceptions.push(t("2026-11-03 09:00"));
        rule.exceptions.push(t("2026-11-04 09:00"));
        let series = Event { recurrence: Some(rule), ..event_at("s", "2026-11-02 09:00", "2026-11-02 09:30") };
        let detached = Event { recurring_id: Some("s".into()), ..event_at("d", "2026-11-04 15:00", "2026-11-04 15:30") };
        let s = store(vec![series, detached]);
        let first = recurrence::occurrence_id("s", t("2026-11-02 09:00"));
        assert_eq!(s.adjacent_occurrence(&first, true).unwrap().id, "d");
        assert_eq!(s.adjacent_occurrence("d", true).unwrap().start, t("2026-11-05 09:00"));
        assert_eq!(s.adjacent_occurrence("d", false).unwrap().id, first);
    }

    #[test]
    fn steps_through_occurrences_google_sent_one_by_one() {
        let occ = |id: &str, day: &str| Event {
            recurring_id: Some("g".into()),
            ..event_at(id, &format!("{day} 10:00"), &format!("{day} 11:00"))
        };
        // Same series id on another calendar: a different series.
        let other = Event { calendar_id: "local:work".into(), ..occ("x", "2026-11-05") };
        let s = store(vec![occ("a", "2026-11-01"), occ("c", "2026-11-15"), occ("b", "2026-11-08"), other]);
        assert_eq!(s.adjacent_occurrence("a", true).unwrap().id, "b");
        assert_eq!(s.adjacent_occurrence("b", true).unwrap().id, "c");
        assert_eq!(s.adjacent_occurrence("c", false).unwrap().id, "b");
        assert!(s.adjacent_occurrence("c", true).is_none());
    }

    #[test]
    fn single_events_have_no_occurrences() {
        let s = store(vec![event_at("e", "2026-11-02 09:00", "2026-11-02 10:00")]);
        assert!(s.adjacent_occurrence("e", true).is_none());
        assert!(s.adjacent_occurrence("e", false).is_none());
    }

    #[test]
    fn copies_are_new_single_events() {
        let ev = Event {
            remote_id: Some("r".into()),
            recurring_id: Some("g".into()),
            etag: Some("e".into()),
            pending: true,
            rev: 4,
            recurrence: Some(Recurrence::new(Freq::Daily)),
            time_zone: Some("Europe/Amsterdam".into()),
            ..event_at("e", "2026-11-02 09:00", "2026-11-02 10:30")
        };
        let c = ev.copy_as_new("local:work".into(), t("2026-11-20 14:00"));
        assert_ne!(c.id, ev.id);
        assert!(c.id.starts_with("local:"));
        assert_eq!((c.start, c.end), (t("2026-11-20 14:00"), t("2026-11-20 15:30")));
        assert_eq!((c.title.as_str(), c.location.as_str(), c.calendar_id.as_str()), ("e", "Room 1", "local:work"));
        assert_eq!(c.time_zone.as_deref(), Some("Europe/Amsterdam"));
        assert!(c.remote_id.is_none() && c.recurring_id.is_none() && c.etag.is_none() && c.recurrence.is_none());
        assert!(!c.pending && c.rev == 0);
    }

    #[test]
    fn email_check() {
        for ok in ["a@b.co", "first.last+tag@mail.example.org", " x@y.z "] {
            assert!(looks_like_email(ok), "{ok}");
        }
        for bad in ["", "a", "a@", "@b.c", "a@b", "a@b.", "a b@c.d", "a@b@c.d", "a@b.c, d@e.f"] {
            assert!(!looks_like_email(bad), "{bad}");
        }
    }

    /// A one-hour timed event at 09:00 on 2026-11-14.
    pub fn event(id: &str, calendar_id: &str, title: &str) -> Event {
        let start = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap().and_hms_opt(9, 0, 0).unwrap();
        Event {
            id: id.into(),
            calendar_id: calendar_id.into(),
            title: title.into(),
            start,
            end: start + Duration::hours(1),
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

    #[test]
    fn calendars_know_their_account() {
        let cal = |source| Calendar { id: "x".into(), name: "x".into(), color: [0; 3], visible: true, source, read_only: false };
        assert_eq!(cal(Source::Local).remote(), None);
        assert_eq!(cal(Source::Google { calendar_id: "me@x".into() }).remote(), Some(CalRef::google("me@x")));
        let dav = cal(Source::CalDav { account: "caldav-1".into(), url: "https://d/cal/".into() });
        assert_eq!(dav.remote(), Some(CalRef::new("caldav-1", "https://d/cal/")));
        assert_eq!(dav.account(), Some("caldav-1"));
    }

    #[test]
    fn signed_out_accounts_have_no_login() {
        let mut store = Store::with_defaults();
        assert!(store.accounts().is_empty());
        store.caldav.push(CalDavAccount {
            id: "caldav-1".into(),
            label: "iCloud".into(),
            server: "https://caldav.icloud.com/".into(),
            username: "me@icloud.com".into(),
            password: None,
        });
        assert!(store.login("caldav-1").is_none(), "no password, no login");
        store.caldav[0].password = Some("abcd-efgh-ijkl-mnop".into());
        store.google.refresh_token = Some("r".into());
        assert_eq!(store.accounts(), ["google", "caldav-1"]);
        assert!(matches!(store.login("caldav-1"), Some(Login::CalDav(l)) if l.username == "me@icloud.com"));
        assert!(matches!(store.login("google"), Some(Login::Google(_))));
        assert_eq!(store.account_name("caldav-1"), "iCloud");
        assert_eq!(store.account_name("google"), "Google");
    }

    #[test]
    fn old_pending_deletes_are_google_ones() {
        let d: PendingDelete =
            serde_json::from_str(r#"{"google_calendar_id": "me@x", "remote_id": "e1", "etag": "\"v1\""}"#).unwrap();
        assert_eq!(d.calendar, CalRef::google("me@x"));
        assert_eq!((d.remote_id.as_str(), d.etag.as_deref()), ("e1", Some("\"v1\"")));
    }
}
