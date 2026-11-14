//! Core data model: calendars, events, and the on-disk store.

use std::collections::HashMap;
use std::path::PathBuf;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
        }
    }

    pub fn set_fields(&mut self, f: EventFields) {
        self.title = f.title;
        self.location = f.location;
        self.notes = f.notes;
        self.start = f.start;
        self.end = f.end;
        self.all_day = f.all_day;
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    /// Google's `nextSyncToken`; `None` forces a full sync.
    pub token: Option<String>,
    /// When the last full sync happened.
    pub full_sync_on: Option<NaiveDate>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
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
}

impl Store {
    fn path() -> PathBuf {
        if let Some(dir) = std::env::var_os("CLEANCALENDAR_DATA") {
            return PathBuf::from(dir).join("store.json");
        }
        directories::ProjectDirs::from("com", "CleanCalendar", "CleanCalendar")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join("store.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(store) => return store,
                Err(e) => {
                    eprintln!("Could not parse {}: {e}", path.display());
                    let backup = path.with_extension("json.bak");
                    let _ = std::fs::copy(&path, backup);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("Could not read {}: {e}", path.display()),
        }
        Self::with_defaults()
    }

    fn with_defaults() -> Self {
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
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
                    eprintln!("Could not save {}: {e}", path.display());
                }
            }
            Err(e) => eprintln!("Could not serialize store: {e}"),
        }
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
