//! Core data model: calendars, events, and the in-memory store (saved by `db`).

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::recurrence::{self, Recurrence};
use crate::remote::{CalRef, Login, GOOGLE};
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

    /// The remote calendar an event lives on, if any.
    pub fn remote_of(&self, ev: &Event) -> Option<CalRef> {
        self.calendar(&ev.calendar_id).and_then(Calendar::remote)
    }

    /// The signed-in accounts, Google first.
    pub fn accounts(&self) -> Vec<String> {
        let google = self.google.is_connected().then(|| GOOGLE.to_owned());
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

    pub fn is_visible(&self, calendar_id: &str) -> bool {
        self.calendar(calendar_id).is_some_and(|c| c.visible)
    }

    pub fn writable_calendars(&self) -> impl Iterator<Item = &Calendar> {
        self.calendars.iter().filter(|c| !c.read_only)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

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
