//! Tests against real Google Calendar, using a test account.
//!
//! Ignored by default; they need an OAuth client and a refresh token in the
//! environment (never commit these):
//!
//! ```text
//! CLEANCALENDAR_LIVE_CLIENT_ID=...  CLEANCALENDAR_LIVE_CLIENT_SECRET=...
//! # one-time, opens the browser to approve access and saves the token:
//! CLEANCALENDAR_LIVE_TOKEN_FILE=path/outside/repo cargo test live_sign_in -- --ignored --nocapture
//! CLEANCALENDAR_LIVE_REFRESH_TOKEN=... cargo test live_tests -- --ignored --test-threads 1
//! ```
//!
//! Each test creates its own temporary calendar and deletes it afterwards,
//! then plays two copies of the app ("laptop" and "phone") against it, each
//! with its own store and sync engine: the same as launching the app twice,
//! but scripted so conflicts can be created on purpose.

use chrono::{Duration, Local, NaiveDate, NaiveDateTime};
use serde_json::json;

use super::*;
use crate::model::{GoogleSettings, Store};
use crate::sync::{Notice, SyncEngine};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} (see src/google/live_tests.rs)"))
}

fn creds() -> Creds {
    Creds {
        client_id: env("CLEANCALENDAR_LIVE_CLIENT_ID"),
        client_secret: env("CLEANCALENDAR_LIVE_CLIENT_SECRET"),
        refresh_token: env("CLEANCALENDAR_LIVE_REFRESH_TOKEN"),
    }
}

fn no_browser(_: &str) -> Result<()> {
    panic!("only live_sign_in opens a browser")
}

/// One-time interactive sign-in: opens the browser, waits for approval, and
/// writes the refresh token to CLEANCALENDAR_LIVE_TOKEN_FILE (never prints it).
#[test]
#[ignore = "interactive: opens a browser to approve access"]
fn live_sign_in() {
    let path = env("CLEANCALENDAR_LIVE_TOKEN_FILE");
    let browser = |url: &str| open::that(url).context("opening the browser");
    let token = authorize(
        &Endpoints::google(),
        &env("CLEANCALENDAR_LIVE_CLIENT_ID"),
        &env("CLEANCALENDAR_LIVE_CLIENT_SECRET"),
        &browser,
    )
    .expect("sign-in");
    std::fs::write(&path, token).expect("write token file");
    println!("Signed in; token saved.");
}

// ---------------------------------------------------------------------------
// Harness

/// Raw API call for things the app itself doesn't do (calendars, recurring
/// events), through the same client and auth.
fn raw(c: &mut Client, method: reqwest::Method, segments: &[&str], body: Option<Value>) -> Result<Value> {
    let mut req = c.http.request(method, c.url(segments)).bearer_auth(c.token()?);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = Client::check(req.send()?)?;
    let text = resp.text()?;
    Ok(if text.is_empty() { Value::Null } else { serde_json::from_str(&text)? })
}

/// `raw` with a few retries on transient failures (dropped connections,
/// rate limits), for test setup and cleanup that must not flake.
fn raw_retry(method: reqwest::Method, segments: &[&str], body: Option<Value>) -> Result<Value> {
    let mut c = Client::new(creds(), &Endpoints::google());
    let mut attempt = 0;
    loop {
        match raw(&mut c, method.clone(), segments, body.clone()) {
            Err(e) if classify(&e) == Failure::Retry && attempt < 3 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(500 * attempt));
            }
            result => return result,
        }
    }
}

/// A temporary calendar, deleted when dropped (even if the test fails).
struct TempCalendar {
    id: String,
}

impl TempCalendar {
    fn create() -> Self {
        let name = format!("CleanCalendar test {}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let cal = raw_retry(reqwest::Method::POST, &["calendars"], Some(json!({ "summary": name }))).expect("create calendar");
        TempCalendar { id: cal["id"].as_str().unwrap().to_owned() }
    }
}

impl Drop for TempCalendar {
    fn drop(&mut self) {
        if let Err(e) = raw_retry(reqwest::Method::DELETE, &["calendars", &self.id], None) {
            eprintln!("could not delete test calendar {}: {e:#}", self.id);
        }
    }
}

/// One running copy of the app, minus window and thread.
struct Device {
    name: &'static str,
    store: Store,
    engine: SyncEngine,
    client: Option<Client>,
    cal: String,
    today: NaiveDate,
    /// Notices the engine reported, for assertions.
    said: Vec<String>,
}

impl Device {
    fn new(name: &'static str, cal: &TempCalendar) -> Self {
        let c = creds();
        let mut store = Store::default();
        store.google = GoogleSettings {
            client_id: c.client_id,
            client_secret: c.client_secret,
            refresh_token: Some(c.refresh_token),
            account_email: None,
        };
        Device {
            name,
            store,
            engine: SyncEngine::default(),
            client: None,
            cal: cal.id.clone(),
            today: Local::now().date_naive(),
            said: Vec::new(),
        }
    }

    /// Delivers queued commands to Google until nothing is left.
    fn pump(&mut self) {
        loop {
            let cmds: Vec<Cmd> = self.engine.outbox.drain(..).collect();
            if cmds.is_empty() {
                break;
            }
            for cmd in cmds {
                let msg = run(&mut self.client, &Endpoints::google(), &no_browser, cmd);
                self.engine.handle(&mut self.store, msg, None, self.today);
            }
            for n in self.engine.notices.drain(..) {
                if let Notice::Status { text, .. } = n {
                    println!("[{}] {text}", self.name);
                    self.said.push(text);
                }
            }
        }
    }

    fn sync(&mut self) {
        assert!(self.engine.sync(&self.store, self.today), "{}: sync should start", self.name);
        self.pump();
    }

    /// Takes changes without sending them yet (like being offline).
    fn create_offline(&mut self, title: &str, start: NaiveDateTime, minutes: i64) -> String {
        let id = format!("local:{}", uuid::Uuid::new_v4().simple());
        self.store.events.push(Event {
            id: id.clone(),
            calendar_id: local_calendar_id(&self.cal),
            title: title.into(),
            start,
            end: start + Duration::minutes(minutes),
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
        });
        self.engine.event_changed(&mut self.store, &id);
        id
    }

    fn create(&mut self, title: &str, start: NaiveDateTime, minutes: i64) {
        self.create_offline(title, start, minutes);
        self.pump();
    }

    /// Edits an event the way the editor does; `pump()` sends it.
    fn edit(&mut self, title: &str, change: impl FnOnce(&mut Event)) {
        let id = self.find(title).id.clone();
        self.engine.before_change(&mut self.store, &id);
        change(self.store.event_mut(&id).unwrap());
        self.engine.event_changed(&mut self.store, &id);
    }

    fn delete(&mut self, title: &str) {
        let ev = self.find(title).clone();
        self.store.events.retain(|e| e.id != ev.id);
        self.engine.event_deleted(&mut self.store, &ev, true);
    }

    /// Events on the test calendar.
    fn events(&self) -> Vec<&Event> {
        let cal = local_calendar_id(&self.cal);
        let mut v: Vec<&Event> = self.store.events.iter().filter(|e| e.calendar_id == cal).collect();
        v.sort_by_key(|e| (e.start, e.title.clone()));
        v
    }

    fn titles(&self) -> Vec<String> {
        self.events().iter().map(|e| e.title.clone()).collect()
    }

    fn find(&self, title: &str) -> &Event {
        let found: Vec<&Event> = self.events().into_iter().filter(|e| e.title == title).collect();
        assert_eq!(found.len(), 1, "{}: expected one “{title}”, have {:?}", self.name, self.titles());
        found[0]
    }

    fn token(&self) -> Option<String> {
        self.store.sync_state.get(&self.cal).and_then(|s| s.token.clone())
    }
}

/// The test calendar's events as Google has them.
fn google_titles(cal: &TempCalendar) -> Vec<String> {
    let mut c = Client::new(creds(), &Endpoints::google());
    let from = Local::now().date_naive() - Duration::days(60);
    let ch = c.list_events(&cal.id, Query::Window(from, from + Duration::days(400))).unwrap();
    let mut evs = ch.events;
    evs.sort_by_key(|e| (e.start, e.title.clone()));
    evs.into_iter().map(|e| e.title).collect()
}

fn at(days: i64, hour: u32) -> NaiveDateTime {
    (Local::now().date_naive() + Duration::days(days)).and_hms_opt(hour, 0, 0).unwrap()
}

// ---------------------------------------------------------------------------
// Scenarios

#[test]
#[ignore = "talks to real Google; see src/google/live_tests.rs"]
fn live_incremental_sync_sees_creates_edits_and_deletes() {
    let cal = TempCalendar::create();
    let mut laptop = Device::new("laptop", &cal);
    let mut phone = Device::new("phone", &cal);
    laptop.sync();
    phone.sync();
    // Does Google hand out a sync token for a date-limited full sync?
    let first_token = laptop.token();
    println!("sync token after first full sync: {}", if first_token.is_some() { "yes" } else { "no" });
    assert!(first_token.is_some(), "Google returned no sync token for the windowed full sync");

    phone.create("Dentist", at(1, 9), 60);
    laptop.sync();
    assert_eq!(laptop.titles(), ["Dentist"]);
    assert_ne!(laptop.token(), first_token, "incremental sync advanced the token");

    phone.edit("Dentist", |e| e.title = "Dentist (moved)".into());
    phone.pump();
    laptop.sync();
    assert_eq!(laptop.titles(), ["Dentist (moved)"]);

    phone.delete("Dentist (moved)");
    phone.pump();
    laptop.sync();
    assert!(laptop.titles().is_empty(), "deletion arrived incrementally");
}

#[test]
#[ignore = "talks to real Google; see src/google/live_tests.rs"]
fn live_concurrent_edits_to_different_fields_merge() {
    let cal = TempCalendar::create();
    let mut laptop = Device::new("laptop", &cal);
    let mut phone = Device::new("phone", &cal);
    laptop.sync();
    laptop.create("Standup", at(1, 9), 30);
    phone.sync();

    // Laptop is offline and moves it; meanwhile the phone renames it.
    laptop.edit("Standup", |e| {
        e.start = at(1, 10);
        e.end = at(1, 10) + Duration::minutes(30);
    });
    phone.edit("Standup", |e| e.title = "Standup (room 4)".into());
    phone.pump();

    // Laptop comes back online: its upload is refused as stale, then merged.
    laptop.pump();
    let ev = laptop.find("Standup (room 4)");
    assert_eq!(ev.start, at(1, 10), "laptop's move kept");
    assert!(!ev.pending, "merged result accepted by Google");
    assert!(laptop.said.iter().any(|s| s.starts_with("Merged")), "{:?}", laptop.said);

    phone.sync();
    let ev = phone.find("Standup (room 4)");
    assert_eq!(ev.start, at(1, 10), "phone sees the merged event");
    assert_eq!(google_titles(&cal), ["Standup (room 4)"]);
}

#[test]
#[ignore = "talks to real Google; see src/google/live_tests.rs"]
fn live_concurrent_edits_to_the_same_field_keep_the_later_upload() {
    let cal = TempCalendar::create();
    let mut laptop = Device::new("laptop", &cal);
    let mut phone = Device::new("phone", &cal);
    laptop.sync();
    laptop.create("Lunch", at(2, 12), 60);
    phone.sync();

    phone.edit("Lunch", |e| e.title = "Lunch with Sam".into());
    phone.pump();
    laptop.edit("Lunch", |e| {
        e.title = "Lunch with Alex".into();
        e.notes = "book a table".into();
    });
    laptop.pump();

    let ev = laptop.find("Lunch with Alex");
    assert_eq!(ev.notes, "book a table");
    assert!(laptop.said.iter().any(|s| s.contains("kept your title")), "{:?}", laptop.said);
    phone.sync();
    assert_eq!(phone.titles(), ["Lunch with Alex"]);
    assert_eq!(google_titles(&cal), ["Lunch with Alex"]);
}

#[test]
#[ignore = "talks to real Google; see src/google/live_tests.rs"]
fn live_edit_beats_delete_in_both_directions() {
    let cal = TempCalendar::create();
    let mut laptop = Device::new("laptop", &cal);
    let mut phone = Device::new("phone", &cal);
    laptop.sync();
    laptop.create("Review", at(3, 14), 60);
    laptop.create("Retro", at(3, 16), 60);
    phone.sync();

    // 1. Phone deletes "Review" while the laptop edits it: the edit wins.
    phone.delete("Review");
    phone.pump();
    laptop.edit("Review", |e| e.notes = "agenda attached".into());
    laptop.pump();
    assert_eq!(laptop.find("Review").notes, "agenda attached");

    // 2. Laptop deletes "Retro" after the phone edited it: the edit wins.
    phone.edit("Retro", |e| e.location = "Room 2".into());
    phone.pump();
    laptop.delete("Retro");
    laptop.pump();
    assert_eq!(laptop.find("Retro").location, "Room 2", "restored with the phone's change");
    assert!(laptop.said.iter().any(|s| s.contains("wasn't deleted")), "{:?}", laptop.said);

    phone.sync();
    laptop.sync();
    assert_eq!(phone.titles(), ["Review", "Retro"]);
    assert_eq!(laptop.titles(), ["Review", "Retro"]);
    assert_eq!(google_titles(&cal), ["Review", "Retro"]);
}

#[test]
#[ignore = "talks to real Google; see src/google/live_tests.rs"]
fn live_retried_create_does_not_duplicate() {
    let cal = TempCalendar::create();
    let mut laptop = Device::new("laptop", &cal);
    laptop.sync();
    let id = laptop.create_offline("Flight", at(5, 8), 180);
    let Some(Cmd::Save { creds, google_calendar_id, event }) = laptop.engine.outbox.pop() else { panic!("expected a save") };
    // The first attempt reaches Google but "the reply is lost"...
    let lost = run(
        &mut laptop.client,
        &Endpoints::google(),
        &no_browser,
        Cmd::Save { creds: creds.clone(), google_calendar_id: google_calendar_id.clone(), event: event.clone() },
    );
    assert!(matches!(lost, Msg::Saved { .. }));
    // ...so the app retries the same upload.
    let retried = run(&mut laptop.client, &Endpoints::google(), &no_browser, Cmd::Save { creds, google_calendar_id, event });
    match &retried {
        Msg::Saved { remote_id, .. } => assert_eq!(Some(remote_id.as_str()), id.strip_prefix("local:")),
        _ => panic!("retry should resolve to the same event"),
    }
    laptop.engine.handle(&mut laptop.store, retried, None, laptop.today);
    assert_eq!(google_titles(&cal), ["Flight"], "exactly one copy on Google");
    laptop.sync();
    assert_eq!(laptop.titles(), ["Flight"]);
}

#[test]
#[ignore = "talks to real Google; see src/google/live_tests.rs"]
fn live_recurring_events_sync_per_occurrence() {
    let cal = TempCalendar::create();
    let mut laptop = Device::new("laptop", &cal);
    laptop.sync();

    // Another app creates a weekly series of three.
    let mut c = Client::new(creds(), &Endpoints::google());
    let start = at(1, 8);
    let series = raw(
        &mut c,
        reqwest::Method::POST,
        &["calendars", &cal.id, "events"],
        Some(json!({
            "summary": "Gym",
            "start": {"dateTime": to_rfc3339(start), "timeZone": "UTC"},
            "end": {"dateTime": to_rfc3339(start + Duration::hours(1)), "timeZone": "UTC"},
            "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=3"],
        })),
    )
    .unwrap();
    let series_id = series["id"].as_str().unwrap().to_owned();

    laptop.sync();
    assert_eq!(laptop.titles(), ["Gym", "Gym", "Gym"]);
    assert!(laptop.events().iter().all(|e| e.recurring_id.as_deref() == Some(series_id.as_str())));

    // Editing one occurrence changes only that one.
    let second = laptop.events()[1].id.clone();
    laptop.engine.before_change(&mut laptop.store, &second);
    laptop.store.event_mut(&second).unwrap().title = "Gym (legs)".into();
    laptop.engine.event_changed(&mut laptop.store, &second);
    laptop.pump();
    assert_eq!(google_titles(&cal), ["Gym", "Gym (legs)", "Gym"]);

    // Deleting the whole series elsewhere removes every occurrence here.
    raw(&mut c, reqwest::Method::DELETE, &["calendars", &cal.id, "events", &series_id], None).unwrap();
    laptop.sync();
    assert!(laptop.titles().is_empty(), "left: {:?}", laptop.titles());
}
