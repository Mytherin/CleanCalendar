//! End-to-end tests against a running Google emulator (`npx emulate`).
//!
//! These drive the real pieces together: the browser sign-in flow (with a
//! script standing in for the browser), the HTTP client, and the
//! [`SyncEngine`], exactly as the worker thread and the app would. Only the
//! window and the thread are left out.
//!
//! They're ignored by default because they need the emulator:
//!
//! ```text
//! npx emulate@0.13.1 --service google --seed testdata/emulate-seed.yaml
//! CLEANCALENDAR_GOOGLE_URL=http://127.0.0.1:4000 cargo test -- --ignored
//! ```
//!
//! The emulator keeps state for as long as it runs, and tests run in
//! parallel, so every test tags its events with a unique id and only looks
//! at those.

use std::collections::HashMap;

use chrono::{Duration, Local, NaiveDate};
use reqwest::redirect::Policy;

use super::*;
use crate::model::{GoogleSettings, Store};
use crate::sync::SyncEngine;

/// Seeded in testdata/emulate-seed.yaml.
const USER: &str = "tester@example.com";
const WORK: &str = "work@example.com";

fn endpoints() -> Endpoints {
    Endpoints::at(&std::env::var(EMULATOR_ENV).unwrap_or_else(|_| "http://127.0.0.1:4000".into()))
}

fn no_browser(_: &str) -> Result<()> {
    panic!("only sign-in opens a browser")
}

/// Signs in as the seeded user. Instead of a browser, a background thread
/// picks the user on the emulator's sign-in page and follows its redirect
/// back to our loopback listener, which is what a browser would do.
fn sign_in(endpoints: &Endpoints) -> String {
    let base = endpoints.auth.trim_end_matches("/o/oauth2/v2/auth").to_owned();
    let browser = move |url: &str| -> Result<()> {
        let q: HashMap<String, String> = Url::parse(url)?.query_pairs().into_owned().collect();
        let base = base.clone();
        std::thread::spawn(move || {
            let http = reqwest::blocking::Client::builder().redirect(Policy::none()).build().unwrap();
            let field = |k: &str| q.get(k).cloned().unwrap_or_default();
            let picked = http
                .post(format!("{base}/o/oauth2/v2/auth/callback"))
                .form(&[
                    ("email", USER.to_owned()),
                    ("client_id", field("client_id")),
                    ("redirect_uri", field("redirect_uri")),
                    ("scope", field("scope")),
                    ("state", field("state")),
                    ("code_challenge", field("code_challenge")),
                    ("code_challenge_method", field("code_challenge_method")),
                ])
                .send()
                .ok()
                .and_then(|r| Some(r.headers().get("location")?.to_str().ok()?.to_owned()));
            // Like a browser, always land back on the app, so a failure here
            // fails sign-in right away instead of waiting for its timeout.
            let back_to_app = picked.unwrap_or_else(|| {
                format!("{}?error=emulator_sign_in_failed&state={}", field("redirect_uri"), field("state"))
            });
            let _ = http.get(back_to_app).send();
        });
        Ok(())
    };
    authorize(endpoints, "cleancalendar-test", "test-secret", &browser).expect("sign-in")
}

/// The app minus window and thread: a store, the sync engine, and a client
/// that executes the engine's commands one by one.
struct App {
    /// Where this app's requests go (tests may cut it off).
    endpoints: Endpoints,
    /// The emulator itself, for checks and "other device" changes.
    server: Endpoints,
    store: Store,
    engine: SyncEngine,
    client: Option<Client>,
    today: NaiveDate,
    /// Unique marker for this test's events.
    tag: String,
}

impl App {
    fn signed_in() -> Self {
        let endpoints = endpoints();
        let refresh_token = sign_in(&endpoints);
        let mut store = Store::default();
        store.google = GoogleSettings {
            client_id: "cleancalendar-test".into(),
            client_secret: "test-secret".into(),
            refresh_token: Some(refresh_token),
            account_email: None,
        };
        App {
            server: endpoints.clone(),
            endpoints,
            store,
            engine: SyncEngine::default(),
            client: None,
            today: Local::now().date_naive(),
            tag: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
        }
    }

    /// Runs queued commands (and whatever they queue in turn) to completion.
    fn pump(&mut self) {
        loop {
            let cmds: Vec<Cmd> = self.engine.outbox.drain(..).collect();
            if cmds.is_empty() {
                break;
            }
            for cmd in cmds {
                let msg = run(&mut self.client, &self.endpoints, &no_browser, cmd);
                self.engine.handle(&mut self.store, msg, None, self.today);
            }
        }
    }

    fn sync(&mut self) {
        assert!(self.engine.sync(&self.store, self.today), "sync should start");
        self.pump();
    }

    fn title(&self, name: &str) -> String {
        format!("{name} [{}]", self.tag)
    }

    /// Creates an event the way the editor does, and lets the engine upload it.
    fn create(&mut self, calendar: &str, name: &str, hour: u32) -> String {
        let start = self.today.and_hms_opt(hour, 0, 0).unwrap();
        let id = format!("local:{}", uuid::Uuid::new_v4().simple());
        self.store.events.push(Event {
            id: id.clone(),
            calendar_id: local_calendar_id(calendar),
            title: self.title(name),
            start,
            end: start + Duration::minutes(45),
            all_day: false,
            location: "Room 1".into(),
            notes: "agenda".into(),
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

    fn local(&self, name: &str) -> Vec<&Event> {
        let title = self.title(name);
        self.store.events.iter().filter(|e| e.title == title).collect()
    }

    fn mine_locally(&self) -> usize {
        self.store.events.iter().filter(|e| e.title.contains(&self.tag)).count()
    }

    /// A second, independent client: "another device" editing the calendar.
    fn other_device(&self) -> Client {
        let creds = Creds {
            client_id: "cleancalendar-test".into(),
            client_secret: "test-secret".into(),
            refresh_token: sign_in(&self.server),
        };
        Client::new(creds, &self.server)
    }

    /// This test's events as Google (the emulator) has them.
    fn remote(&self, calendar: &str) -> Vec<Event> {
        let mut c = self.other_device();
        let from = self.today - Duration::days(30);
        let ch = c.list_events(calendar, Query::Window(from, self.today + Duration::days(30))).unwrap();
        ch.events.into_iter().filter(|e| e.title.contains(&self.tag)).collect()
    }
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn sign_in_and_first_sync_import_the_calendars() {
    let mut app = App::signed_in();
    app.sync();

    let mut google: Vec<(&str, bool)> =
        app.store.calendars.iter().filter_map(|c| Some((c.google_id()?, c.read_only))).collect();
    google.sort();
    assert_eq!(google, [(USER, false), (WORK, false)]);
    assert_eq!(app.store.google.account_email.as_deref(), Some(USER));
    // The emulator doesn't implement sync tokens, so each sync stays a full one.
    assert_eq!(app.store.sync_state[USER].token, None);
    assert_eq!(app.store.unsynced_count(), 0);
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn new_event_is_uploaded_once_and_survives_resync() {
    let mut app = App::signed_in();
    app.sync();
    app.create(USER, "Planning", 10);
    app.pump();

    let local = app.local("Planning");
    assert_eq!(local.len(), 1);
    assert!(!local[0].pending, "confirmed by Google");
    let remote_id = local[0].remote_id.clone().expect("got a Google id");
    let start = local[0].start;

    let remote = app.remote(USER);
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].remote_id.as_deref(), Some(remote_id.as_str()));
    assert_eq!((remote[0].start, remote[0].location.as_str(), remote[0].notes.as_str()), (start, "Room 1", "agenda"));

    // Syncing again must not duplicate it.
    app.sync();
    app.sync();
    assert_eq!(app.local("Planning").len(), 1);
    assert_eq!(app.remote(USER).len(), 1);
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn changes_from_another_device_show_up_on_next_sync() {
    let mut app = App::signed_in();
    app.sync();

    let mut phone = app.other_device();
    let start = app.today.and_hms_opt(15, 0, 0).unwrap();
    let ev = Event {
        id: "unused".into(),
        calendar_id: "unused".into(),
        title: app.title("From phone"),
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
    };
    let remote_id = phone.save_event(WORK, &ev).unwrap().0;
    app.sync();
    let local = app.local("From phone");
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].calendar_id, local_calendar_id(WORK));
    assert_eq!(local[0].start, start);

    phone.delete_event(WORK, &remote_id, None).unwrap();
    app.sync();
    assert!(app.local("From phone").is_empty(), "deleted elsewhere, gone here");
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn deleting_locally_deletes_on_google() {
    let mut app = App::signed_in();
    app.sync();
    let id = app.create(WORK, "Cancelled meeting", 11);
    app.pump();
    let id = app.store.events.iter().find(|e| e.title.contains(&app.tag)).map(|e| e.id.clone()).unwrap_or(id);
    assert_eq!(app.remote(WORK).len(), 1);

    let ev = app.store.event(&id).unwrap().clone();
    app.store.events.retain(|e| e.id != id);
    app.engine.event_deleted(&mut app.store, &ev, true);
    app.pump();

    assert!(app.remote(WORK).is_empty());
    assert!(app.store.pending_deletes.is_empty(), "delete confirmed");
    app.sync();
    assert_eq!(app.mine_locally(), 0, "doesn't come back");
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn changes_made_offline_upload_when_back_online() {
    let mut app = App::signed_in();
    app.sync();

    // Offline: point the app at a port nothing listens on.
    app.endpoints = Endpoints::at("http://127.0.0.1:9");
    app.client = None;
    app.create(USER, "Written on a plane", 9);
    app.pump();
    assert!(app.local("Written on a plane")[0].pending);
    assert!(app.remote(USER).is_empty(), "nothing reached Google");
    app.engine.handle(&mut app.store, Msg::SyncFailed("offline".into()), None, app.today);

    // Back online: the next sync uploads it first.
    app.endpoints = app.server.clone();
    app.client = None;
    app.sync();
    let local = app.local("Written on a plane");
    assert_eq!(local.len(), 1);
    assert!(!local[0].pending);
    assert_eq!(app.remote(USER).len(), 1);
    assert_eq!(app.store.unsynced_count(), 0);
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn all_day_events_round_trip() {
    let mut app = App::signed_in();
    app.sync();
    let start = app.today.and_hms_opt(0, 0, 0).unwrap();
    let id = format!("local:{}", uuid::Uuid::new_v4().simple());
    app.store.events.push(Event {
        id: id.clone(),
        calendar_id: local_calendar_id(USER),
        title: app.title("Offsite"),
        start,
        end: start + Duration::days(2),
        all_day: true,
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
    app.engine.event_changed(&mut app.store, &id);
    app.pump();

    let remote = app.remote(USER);
    assert_eq!(remote.len(), 1);
    assert!(remote[0].all_day);
    assert_eq!((remote[0].start, remote[0].end), (start, start + Duration::days(2)));
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn full_sync_reads_every_page() {
    let mut app = App::signed_in();
    // The emulator returns at most 250 events per page.
    let mut phone = app.other_device();
    let calendar = WORK;
    for i in 0..260 {
        let start = (app.today - Duration::days(i % 20)).and_hms_opt(7, 0, 0).unwrap();
        let ev = Event {
            id: "unused".into(),
            calendar_id: "unused".into(),
            title: app.title(&format!("Bulk {i}")),
            start,
            end: start + Duration::minutes(10),
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
        };
        phone.save_event(calendar, &ev).unwrap();
    }
    app.sync();
    assert_eq!(app.mine_locally(), 260);
}

/// Canary: the emulator can't update events yet, so edits aren't covered
/// here. When this starts failing, the emulator gained PATCH support and an
/// edit round-trip test should be added.
#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn emulator_still_lacks_event_updates() {
    let app = App::signed_in();
    let mut phone = app.other_device();
    let start = app.today.and_hms_opt(8, 0, 0).unwrap();
    let mut ev = Event {
        id: "unused".into(),
        calendar_id: "unused".into(),
        title: app.title("Patch me"),
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
    };
    let remote_id = phone.save_event(USER, &ev).unwrap().0;
    ev.remote_id = Some(remote_id.clone());
    ev.title = app.title("Patched");
    let err = phone.save_event(USER, &ev).unwrap_err();
    assert_eq!(http_status(&err), Some(404));
    phone.delete_event(USER, &remote_id, None).unwrap();
}

#[test]
#[ignore = "needs the Google emulator; see src/google/emulator_tests.rs"]
fn our_edit_beats_a_delete_on_another_device() {
    let mut app = App::signed_in();
    app.sync();
    app.create(WORK, "Keep me", 13);
    app.pump();
    let ev = app.local("Keep me")[0].clone();
    let remote_id = ev.remote_id.clone().unwrap();

    // The phone deletes it while we edit it.
    app.other_device().delete_event(WORK, &remote_id, None).unwrap();
    app.engine.before_change(&mut app.store, &ev.id);
    app.store.event_mut(&ev.id).unwrap().title = app.title("Keep me (edited)");
    app.engine.event_changed(&mut app.store, &ev.id);
    app.pump();

    // Recreated on Google with our edit; exactly one copy on each side.
    let remote = app.remote(WORK);
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].title, app.title("Keep me (edited)"));
    assert_ne!(remote[0].remote_id.as_deref(), Some(remote_id.as_str()));
    app.sync();
    assert_eq!(app.local("Keep me (edited)").len(), 1);
    assert_eq!(app.mine_locally(), 1);
}
