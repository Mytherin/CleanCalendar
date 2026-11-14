//! Core data model: calendars, events, and the in-memory store (saved by `db`).

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

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
    pub fn google_id(&self) -> Option<&str> {
        match &self.source {
            Source::Google { calendar_id } => Some(calendar_id),
            Source::Local => None,
        }
    }
}

/// An event. Times are naive local wall-clock times. For all-day events
/// `start` is midnight of the first day and `end` is midnight of the day
/// *after* the last day (exclusive), matching Google's convention.
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
        self.attendees = f.attendees;
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

/// A remote deletion that hasn't been confirmed yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingDelete {
    pub google_calendar_id: String,
    pub remote_id: String,
    /// Delete only if the server still has this version (`None`: always).
    #[serde(default)]
    pub etag: Option<String>,
}

/// Per-calendar incremental sync bookmark.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    /// Google's `nextSyncToken`; `None` forces a full sync.
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
    pub pending_deletes: Vec<PendingDelete>,
    /// Keyed by Google calendar id.
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

    /// The Google calendar id an event lives on, if any.
    pub fn google_calendar_of(&self, ev: &Event) -> Option<String> {
        self.calendar(&ev.calendar_id).and_then(|c| c.google_id()).map(str::to_owned)
    }

    /// Number of local changes not yet confirmed by Google.
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
mod tests {
    use super::*;

    #[test]
    fn email_check() {
        for ok in ["a@b.co", "first.last+tag@mail.example.org", " x@y.z "] {
            assert!(looks_like_email(ok), "{ok}");
        }
        for bad in ["", "a", "a@", "@b.c", "a@b", "a@b.", "a b@c.d", "a@b@c.d", "a@b.c, d@e.f"] {
            assert!(!looks_like_email(bad), "{bad}");
        }
    }
}
