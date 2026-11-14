//! The CalDAV client against the in-memory server ([`fake`]): first the
//! client on its own, then end to end with the [`SyncEngine`] as the app
//! drives it, with two "devices" (each its own store and engine, like
//! running the app on a laptop and a phone) sharing one account.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDateTime, Timelike};

use super::fake::{self, FakeDav};
use super::*;
use crate::model::{CalDavAccount, Store};
use crate::recurrence::{Freq, Recurrence, RepeatEnd};
use crate::remote::{run, sync_all, Cmd, Connections, Failure, Login as AnyLogin, Msg};
use crate::sync::{Notice, SyncEngine};

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()
}

fn at(day: u32, hour: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(hour, 0, 0).unwrap()
}

fn window() -> (NaiveDate, NaiveDate) {
    (NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(), NaiveDate::from_ymd_opt(2027, 12, 1).unwrap())
}

const ACCOUNT: &str = "caldav-test";

fn calref(server: &FakeDav, name: &str) -> CalRef {
    CalRef::new(ACCOUNT, &server.cal_url(name))
}

fn plain_ics(uid: &str, title: &str, start: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Other//EN\nBEGIN:VEVENT\nUID:{uid}\nSUMMARY:{title}\n\
         DTSTART;TZID=Europe/Amsterdam:{start}\nDURATION:PT1H\nEND:VEVENT\nEND:VCALENDAR\n"
    )
}

// ---------------------------------------------------------------------------
// The client

#[test]
fn discovery_follows_the_principal_to_the_partition_host() {
    let server = FakeDav::start();
    let mut c = Client::new(server.login());
    let cals = c.calendars().unwrap();

    let names: Vec<&str> = cals.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Home", "Work", "US Holidays"], "in calendar-order; no Reminders list, no inbox");
    assert_eq!(cals[0].id, server.cal_url("home"));
    assert_eq!(cals[0].color, [0xFF, 0x29, 0x68]);
    assert!(cals[2].read_only && !cals[0].read_only);
    assert!(cals[0].primary && !cals[1].primary);

    let reqs = server.requests();
    let trail: Vec<(&str, &str, &str)> =
        reqs.iter().map(|r| (r.host, r.method.as_str(), r.path.as_str())).collect();
    assert_eq!(
        trail,
        [
            ("front", "PROPFIND", "/"),
            ("front", "PROPFIND", "/1234/principal/"),
            ("partition", "PROPFIND", "/1234/calendars/"),
        ]
    );
    assert_eq!(reqs[2].header("depth"), Some("1"));
    assert!(reqs.iter().all(|r| r.header("user-agent").is_some_and(|u| u.starts_with("CleanCalendar/"))));

    // The home is remembered: listing again goes straight there.
    server.clear_log();
    c.calendars().unwrap();
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_wrong_password_is_reported_with_advice() {
    let server = FakeDav::start();
    let mut conns = Connections::new(crate::google::Endpoints::google(), Box::new(|_: &str| panic!()));
    let mut login = server.login();
    login.password = "my-apple-id-password".into();
    match run(&mut conns, Cmd::ConnectCalDav { account: ACCOUNT.into(), login }) {
        Msg::ConnectFailed { error, .. } => assert!(error.contains("app-specific password"), "{error}"),
        m => panic!("{m:?}"),
    }
}

#[test]
fn full_then_incremental_sync() {
    let server = FakeDav::start();
    server.put("home", "a.ics", &plain_ics("a", "Dentist", "20261114T090000"));
    server.put("home", "b.ics", &plain_ics("b", "Lunch", "20261115T120000"));
    let mut c = Client::new(server.login());
    let cal = calref(&server, "home");

    let full = c.events(&cal, Query::Window(window().0, window().1), window()).unwrap();
    assert!(full.full);
    let mut titles: Vec<&str> = full.events.iter().map(|e| e.title.as_str()).collect();
    titles.sort();
    assert_eq!(titles, ["Dentist", "Lunch"]);
    assert_eq!(full.events[0].remote_id.as_deref(), Some("/1234/calendars/home/a.ics"));
    let token = full.next_token.clone().expect("a sync token");

    // Another device changes one, deletes one, adds one.
    server.put("home", "a.ics", &plain_ics("a", "Dentist (moved)", "20261116T090000"));
    server.remove("home", "b.ics");
    server.put("home", "c.ics", &plain_ics("c", "Gym", "20261117T070000"));
    server.clear_log();
    let ch = c.events(&cal, Query::Changes(&token), window()).unwrap();
    assert!(!ch.full);
    let mut titles: Vec<&str> = ch.events.iter().map(|e| e.title.as_str()).collect();
    titles.sort();
    assert_eq!(titles, ["Dentist (moved)", "Gym"]);
    let mut cancelled = ch.cancelled.clone();
    cancelled.sort();
    assert_eq!(cancelled, ["/1234/calendars/home/a.ics", "/1234/calendars/home/b.ics", "/1234/calendars/home/c.ics"]);
    let kinds: Vec<String> = server.requests().iter().map(|r| format!("{} {}", r.method, r.body.contains("calendar-multiget"))).collect();
    assert_eq!(kinds, ["REPORT false", "REPORT true"], "sync-collection, then one multiget");
    assert_ne!(ch.next_token.as_deref(), Some(token.as_str()));

    // No changes: nothing to fetch.
    server.clear_log();
    let none = c.events(&cal, Query::Changes(ch.next_token.as_deref().unwrap()), window()).unwrap();
    assert!(none.events.is_empty() && none.cancelled.is_empty());
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn an_expired_token_means_a_full_sync() {
    let server = FakeDav::start();
    server.put("home", "a.ics", &plain_ics("a", "Dentist", "20261114T090000"));
    let mut c = Client::new(server.login());
    let cal = calref(&server, "home");
    let token = c.events(&cal, Query::Window(window().0, window().1), window()).unwrap().next_token.unwrap();
    server.expire_tokens();
    let err = c.events(&cal, Query::Changes(&token), window()).unwrap_err();
    assert!(err.is::<TokenExpired>());

    let tokens = HashMap::from([(cal.id.clone(), token)]);
    let (_, changes) = sync_all(&mut c, ACCOUNT, window().0, window().1, &tokens).unwrap();
    let home = changes.iter().find(|ch| ch.calendar_id == cal.id).unwrap();
    assert!(home.full && home.events.len() == 1);
}

fn new_event(server: &FakeDav, cal: &str, local: &str, title: &str) -> Event {
    let mut ev = crate::model::tests::event(local, &calref(server, cal).local_id(), title);
    ev.time_zone = Some("Europe/Amsterdam".into());
    ev
}

#[test]
fn creating_puts_a_new_resource_once() {
    let server = FakeDav::start();
    let mut c = Client::new(server.login());
    let cal = calref(&server, "home");
    let ev = new_event(&server, "home", "local:0a1b2c", "Flight");

    let (href, etag) = c.save(&cal, &ev).unwrap();
    assert_eq!(href, "/1234/calendars/home/0a1b2c.ics");
    assert!(etag.is_some());
    let put = server.requests().into_iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!(put.header("if-none-match"), Some("*"), "never overwrites");
    assert!(put.header("content-type").unwrap().starts_with("text/calendar"));
    assert!(put.body.contains("UID:0a1b2c"));

    // The reply was lost and the app retries: same resource, no duplicate.
    let (again, etag2) = c.save(&cal, &ev).unwrap();
    assert_eq!((again, etag2), (href, etag));
    assert_eq!(server.files("home").len(), 1);
}

#[test]
fn servers_that_leave_out_the_etag_are_asked_for_it() {
    let server = FakeDav::start();
    server.with(|s| s.etag_on_put = false);
    let mut c = Client::new(server.login());
    let (_, etag) = c.save(&calref(&server, "home"), &new_event(&server, "home", "local:77", "x")).unwrap();
    assert_eq!(etag.as_deref(), Some("\"77-2\""));
}

#[test]
fn updates_are_conditional_and_stale_ones_become_conflicts() {
    let server = FakeDav::start();
    let etag = server.put("home", "a.ics", &plain_ics("a", "Dentist", "20261114T090000"));
    let cal = calref(&server, "home");
    let mut c = Client::new(server.login());
    let mut ev = c.fetch(&cal, "/1234/calendars/home/a.ics").unwrap().unwrap();
    assert_eq!(ev.etag.as_deref(), Some(etag.as_str()));
    ev.base = Some(ev.fields());
    ev.title = "Dentist!".into();
    let (_, new_etag) = c.save(&cal, &ev).unwrap();
    let put = server.requests().into_iter().rfind(|r| r.method == "PUT").unwrap();
    assert_eq!(put.header("if-match"), Some(etag.as_str()));
    assert!(server.file("home", "a.ics").unwrap().contains("SUMMARY:Dentist!"));

    // Someone else changes it; our next edit (based on the old version) is
    // refused, and the worker fetches theirs.
    server.put("home", "a.ics", &plain_ics("a", "Dentist (phone)", "20261114T090000"));
    ev.etag = new_etag;
    ev.base = Some(ev.fields());
    ev.title = "Dentist (laptop)".into();
    let mut conns = Connections::new(crate::google::Endpoints::google(), Box::new(|_: &str| panic!()));
    let msg = run(&mut conns, Cmd::Save { login: AnyLogin::CalDav(server.login()), calendar: cal.clone(), event: ev.clone() });
    match msg {
        Msg::SaveConflict { server: Some(theirs), .. } => assert_eq!(theirs.title, "Dentist (phone)"),
        m => panic!("{m:?}"),
    }
    assert!(server.file("home", "a.ics").unwrap().contains("Dentist (phone)"), "nothing overwritten");

    // Deleted meanwhile: also a conflict, without a copy.
    server.remove("home", "a.ics");
    let msg = run(&mut conns, Cmd::Save { login: AnyLogin::CalDav(server.login()), calendar: cal, event: ev });
    assert!(matches!(msg, Msg::SaveConflict { server: None, .. }), "{msg:?}");
}

#[test]
fn deletes_are_conditional() {
    let server = FakeDav::start();
    let etag = server.put("home", "a.ics", &plain_ics("a", "Dentist", "20261114T090000"));
    let cal = calref(&server, "home");
    let mut c = Client::new(server.login());
    server.put("home", "a.ics", &plain_ics("a", "Changed", "20261114T090000"));
    let err = c.delete(&cal, "/1234/calendars/home/a.ics", Some(&etag)).unwrap_err();
    assert_eq!(http_status(&err), Some(412));
    c.delete(&cal, "/1234/calendars/home/a.ics", None).unwrap();
    assert!(server.files("home").is_empty());
    c.delete(&cal, "/1234/calendars/home/a.ics", None).expect("already gone is fine");
}

#[test]
fn read_only_calendars_refuse_changes() {
    let server = FakeDav::start();
    let mut conns = Connections::new(crate::google::Endpoints::google(), Box::new(|_: &str| panic!()));
    let ev = new_event(&server, "holidays", "local:99", "Mine");
    let msg = run(&mut conns, Cmd::Save { login: AnyLogin::CalDav(server.login()), calendar: calref(&server, "holidays"), event: ev });
    assert!(matches!(msg, Msg::SaveFailed { kind: Failure::Rejected, .. }), "{msg:?}");
}

#[test]
fn editing_an_occurrence_that_no_longer_exists_does_not_duplicate_it() {
    let server = FakeDav::start();
    server.put("home", "yoga.ics", SERIES);
    let (mut laptop, _) = devices(&server);
    // Offline, the laptop edits the first occurrence; meanwhile the phone
    // moves the whole series an hour later, so that occurrence is gone.
    let first = laptop.titled("Yoga")[0].id.clone();
    server.with(|s| s.down = true);
    laptop.edit(&first, |e| e.title = "Yoga with Ana".into());
    laptop.pump();
    server.with(|s| s.down = false);
    server.put("home", "yoga.ics", &SERIES.replace("T070000", "T080000"));
    laptop.sync();
    assert!(laptop.titled("Yoga with Ana").is_empty(), "not recreated as a separate event");
    assert_eq!(laptop.titled("Yoga").len(), 6);
    assert_eq!(server.files("home").len(), 1);
    assert!(laptop.said.iter().any(|s| s.contains("rejected")), "{:?}", laptop.said);
    assert_eq!(laptop.store.unsynced_count(), 0, "not retried forever");
}

#[test]
fn deleting_an_occurrence_someone_just_changed_keeps_it() {
    let server = FakeDav::start();
    server.put("home", "yoga.ics", SERIES);
    let (mut laptop, mut phone) = devices(&server);
    let second = laptop.titled("Yoga")[1].id.clone();
    phone.edit(&second, |e| e.title = "Yoga (outside)".into());
    phone.pump();
    laptop.delete(&second);
    laptop.pump();
    assert!(!server.file("home", "yoga.ics").unwrap().contains("EXDATE"), "not deleted");
    assert_eq!(laptop.titled("Yoga (outside)").len(), 1, "their change came back");

    // An occurrence nobody changed is deleted even though the file changed.
    let third = laptop.titled("Yoga")[1].id.clone();
    laptop.delete(&third);
    laptop.pump();
    assert!(server.file("home", "yoga.ics").unwrap().contains("EXDATE"));
}

#[test]
fn a_calendar_the_server_refuses_does_not_stop_the_others() {
    let server = FakeDav::start();
    server.put("home", "x.ics", &plain_ics("x", "Coffee", "20261118T100000"));
    server.with(|s| s.fail_next.push(("REPORT".into(), "/work/".into(), 403)));
    let laptop = Device::sign_in("laptop", server.login(), today());
    assert_eq!(laptop.titled("Coffee").len(), 1);
    assert!(!laptop.said.iter().any(|s| s.starts_with("Couldn't sync")), "{:?}", laptop.said);
}

#[test]
fn server_errors_are_retryable() {
    let server = FakeDav::start();
    server.with(|s| s.fail_next.push(("PUT".into(), ".ics".into(), 503)));
    let mut conns = Connections::new(crate::google::Endpoints::google(), Box::new(|_: &str| panic!()));
    let ev = new_event(&server, "home", "local:98", "x");
    let msg = run(&mut conns, Cmd::Save { login: AnyLogin::CalDav(server.login()), calendar: calref(&server, "home"), event: ev });
    assert!(matches!(msg, Msg::SaveFailed { kind: Failure::Retry, .. }), "{msg:?}");
}

// ---------------------------------------------------------------------------
// End to end: the sync engine, as the app drives it

/// One copy of the app, minus window and thread.
pub(super) struct Device {
    pub name: &'static str,
    pub store: Store,
    pub engine: SyncEngine,
    pub conns: Connections,
    pub today: NaiveDate,
    /// Status messages the engine showed.
    pub said: Vec<String>,
    pub account: String,
}

impl Device {
    /// Signs in the way the Accounts window does, and runs the first sync.
    pub fn sign_in(name: &'static str, login: Login, today: NaiveDate) -> Self {
        let mut d = Device {
            name,
            store: Store::with_defaults(),
            engine: SyncEngine::default(),
            conns: Connections::new(crate::google::Endpoints::google(), Box::new(|_: &str| panic!("no browser"))),
            today,
            said: Vec::new(),
            account: ACCOUNT.into(),
        };
        d.engine.connect_caldav(CalDavAccount {
            id: ACCOUNT.into(),
            label: "iCloud".into(),
            server: login.server.clone(),
            username: login.username.clone(),
            password: Some(login.password.clone()),
        });
        d.pump();
        assert!(d.store.login(ACCOUNT).is_some(), "[{name}] not signed in: {:?}", d.said);
        d
    }

    /// Runs queued commands (and what they queue in turn) to completion.
    pub fn pump(&mut self) {
        loop {
            let cmds: Vec<Cmd> = self.engine.outbox.drain(..).collect();
            if cmds.is_empty() {
                break;
            }
            for cmd in cmds {
                let msg = run(&mut self.conns, cmd);
                self.engine.handle(&mut self.store, msg, None, self.today);
            }
        }
        for n in self.engine.notices.drain(..) {
            if let Notice::Status { text, .. } = n {
                self.said.push(text);
            }
        }
    }

    pub fn sync(&mut self) {
        self.engine.sync(&self.store, self.today);
        self.pump();
    }

    /// The app's id for a calendar on the server.
    pub fn cal(&self, url: &str) -> String {
        CalRef::new(&self.account, url).local_id()
    }

    /// Adds an event the way the editor does; returns its (first) id.
    pub fn create(&mut self, cal: &str, title: &str, start: NaiveDateTime, minutes: i64) -> String {
        let id = format!("local:{}", uuid::Uuid::new_v4().simple());
        let mut ev = crate::model::tests::event(&id, cal, title);
        ev.start = start;
        ev.end = start + chrono::Duration::minutes(minutes);
        self.store.events.push(ev);
        self.engine.event_changed(&mut self.store, &id);
        id
    }

    /// Changes a stored event the way the editor or a drag does.
    pub fn edit(&mut self, id: &str, f: impl FnOnce(&mut Event)) {
        self.engine.before_change(&mut self.store, id);
        f(self.store.event_mut(id).expect("event to edit"));
        self.engine.event_changed(&mut self.store, id);
    }

    pub fn delete(&mut self, id: &str) {
        let ev = self.store.event(id).cloned().expect("event to delete");
        self.store.events.retain(|e| e.id != id);
        self.engine.event_deleted(&mut self.store, &ev, true);
    }

    pub fn titled(&self, title: &str) -> Vec<&Event> {
        let mut v: Vec<&Event> = self.store.events.iter().filter(|e| e.title == title).collect();
        v.sort_by_key(|e| e.start);
        v
    }

    pub fn one(&self, title: &str) -> &Event {
        let v = self.titled(title);
        assert_eq!(v.len(), 1, "[{}] expected one “{title}”, have {}: {:?}", self.name, v.len(), self.said);
        v[0]
    }
}

fn devices(server: &FakeDav) -> (Device, Device) {
    (Device::sign_in("laptop", server.login(), today()), Device::sign_in("phone", server.login(), today()))
}

#[test]
fn signing_in_imports_the_calendars() {
    let server = FakeDav::start();
    server.put("work", "w.ics", &plain_ics("w", "Review", "20261116T140000"));
    let laptop = Device::sign_in("laptop", server.login(), today());

    let cals: Vec<(&str, bool)> =
        laptop.store.calendars.iter().map(|c| (c.name.as_str(), c.read_only)).collect();
    assert_eq!(cals, [("Home", false), ("Work", false), ("Home", false), ("Work", false), ("US Holidays", true)]);
    assert!(laptop.said.contains(&"Connected to iCloud".to_string()), "{:?}", laptop.said);
    assert_eq!(laptop.one("Review").calendar_id, laptop.cal(&server.cal_url("work")));
    assert_eq!(laptop.store.caldav[0].password.as_deref(), Some(fake::PASSWORD));
    assert!(laptop.store.sync_state.contains_key(&laptop.cal(&server.cal_url("home"))), "token kept");
}

#[test]
fn a_failed_sign_in_adds_nothing() {
    let server = FakeDav::start();
    let mut d = Device {
        name: "laptop",
        store: Store::with_defaults(),
        engine: SyncEngine::default(),
        conns: Connections::new(crate::google::Endpoints::google(), Box::new(|_: &str| panic!())),
        today: today(),
        said: Vec::new(),
        account: ACCOUNT.into(),
    };
    d.engine.connect_caldav(CalDavAccount {
        id: ACCOUNT.into(),
        label: "iCloud".into(),
        server: server.front.clone(),
        username: fake::USER.into(),
        password: Some("wrong".into()),
    });
    assert!(d.engine.is_connecting());
    d.pump();
    assert!(!d.engine.is_connecting());
    assert!(d.store.caldav.is_empty() && d.store.calendars.iter().all(|c| !c.is_remote()));
    assert!(d.said.iter().any(|s| s.contains("app-specific password")), "{:?}", d.said);
}

#[test]
fn an_event_made_on_one_device_shows_up_on_the_other() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    laptop.create(&home, "Flight", at(20, 8), 180);
    laptop.pump();

    let ev = laptop.one("Flight");
    assert!(!ev.pending && ev.etag.is_some());
    assert!(ev.id.starts_with(&home), "renamed to its server id");
    assert_eq!(server.files("home").len(), 1);

    phone.sync();
    let theirs = phone.one("Flight");
    assert_eq!((theirs.id.as_str(), theirs.start, theirs.end), (ev.id.as_str(), ev.start, ev.end));
}

#[test]
fn changes_and_deletions_from_elsewhere_arrive_on_the_next_sync() {
    let server = FakeDav::start();
    let (mut laptop, _) = devices(&server);
    server.put("home", "x.ics", &plain_ics("x", "Coffee", "20261118T100000"));
    server.clear_log();
    laptop.sync();
    assert_eq!(laptop.titled("Coffee").len(), 1);
    assert!(
        !server.requests().iter().any(|r| r.body.contains("calendar-query")),
        "incremental: no full query"
    );

    server.put("home", "x.ics", &plain_ics("x", "Coffee (moved)", "20261119T100000"));
    laptop.sync();
    assert!(laptop.titled("Coffee").is_empty());
    assert_eq!(laptop.one("Coffee (moved)").start.day(), 19);

    server.remove("home", "x.ics");
    laptop.sync();
    assert!(laptop.titled("Coffee (moved)").is_empty());
}

#[test]
fn an_expired_token_resyncs_everything() {
    let server = FakeDav::start();
    let (mut laptop, _) = devices(&server);
    server.put("home", "x.ics", &plain_ics("x", "Coffee", "20261118T100000"));
    server.expire_tokens();
    laptop.sync();
    assert_eq!(laptop.titled("Coffee").len(), 1);
    assert!(!laptop.said.iter().any(|s| s.starts_with("Couldn't sync")), "{:?}", laptop.said);
}

#[test]
fn edits_on_two_devices_are_merged() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    laptop.create(&home, "Standup", at(16, 9), 15);
    laptop.pump();
    phone.sync();
    let id = laptop.one("Standup").id.clone();

    // The phone renames it; the laptop, offline, moves it.
    phone.edit(&id, |e| e.title = "Daily standup".into());
    phone.pump();
    server.with(|s| s.down = true);
    laptop.edit(&id, |e| {
        e.start += chrono::Duration::hours(1);
        e.end += chrono::Duration::hours(1);
    });
    laptop.pump();
    assert!(laptop.store.event(&id).unwrap().pending, "kept while offline");
    assert!(laptop.said.contains(&"Offline — changes will sync when iCloud is reachable".to_string()));

    server.with(|s| s.down = false);
    laptop.sync();
    let merged = laptop.one("Daily standup");
    assert_eq!((merged.start.hour(), merged.pending), (10, false), "both changes kept");
    phone.sync();
    assert_eq!(phone.one("Daily standup").start.hour(), 10);
    let file = &server.files("home")[0].1;
    assert!(file.contains("SUMMARY:Daily standup"));
}

#[test]
fn an_edit_beats_a_delete_on_another_device() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    laptop.create(&home, "Haircut", at(21, 11), 30);
    laptop.pump();
    phone.sync();
    let id = laptop.one("Haircut").id.clone();

    phone.delete(&id);
    phone.pump();
    assert!(server.files("home").is_empty());

    laptop.edit(&id, |e| e.notes = "ask for Sam".into());
    laptop.pump();
    let kept = laptop.one("Haircut");
    assert_eq!(kept.notes, "ask for Sam");
    assert!(!kept.pending, "recreated on the server");
    assert!(laptop.said.iter().any(|s| s.contains("deleted on another device")));
    assert_eq!(server.files("home").len(), 1);
    phone.sync();
    assert_eq!(phone.one("Haircut").notes, "ask for Sam");
}

#[test]
fn a_delete_does_not_win_over_a_newer_edit() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    laptop.create(&home, "Call mum", at(22, 19), 30);
    laptop.pump();
    phone.sync();
    let id = laptop.one("Call mum").id.clone();

    phone.edit(&id, |e| e.title = "Call mum and dad".into());
    phone.pump();
    laptop.delete(&id);
    laptop.pump();
    assert_eq!(server.files("home").len(), 1, "not deleted");
    assert_eq!(laptop.titled("Call mum and dad").len(), 1, "brought back");
}

#[test]
fn moving_an_event_to_another_calendar_moves_it_on_the_server() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    let work = laptop.cal(&server.cal_url("work"));
    laptop.create(&home, "Offsite", at(25, 9), 480);
    laptop.pump();

    // What App::commit_event does for a calendar change.
    let old = laptop.one("Offsite").clone();
    laptop.engine.event_deleted(&mut laptop.store, &old, false);
    {
        let ev = laptop.store.event_mut(&old.id).unwrap();
        ev.calendar_id = work.clone();
        ev.remote_id = None;
        ev.etag = None;
    }
    laptop.engine.event_changed(&mut laptop.store, &old.id);
    laptop.pump();
    assert!(server.files("home").is_empty());
    assert_eq!(server.files("work").len(), 1);
    phone.sync();
    assert_eq!(phone.one("Offsite").calendar_id, work);
}

#[test]
fn all_day_events_round_trip() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    let id = format!("local:{}", uuid::Uuid::new_v4().simple());
    let mut ev = crate::model::tests::event(&id, &home, "Holiday");
    ev.all_day = true;
    ev.start = at(24, 0);
    ev.end = at(27, 0);
    laptop.store.events.push(ev);
    laptop.engine.event_changed(&mut laptop.store, &id);
    laptop.pump();
    assert!(server.files("home")[0].1.contains("DTSTART;VALUE=DATE:20261124"));
    phone.sync();
    let theirs = phone.one("Holiday");
    assert!(theirs.all_day);
    assert_eq!((theirs.start, theirs.end), (at(24, 0), at(27, 0)));
}

const SERIES: &str = "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Apple Inc.//macOS 15//EN\nBEGIN:VEVENT\nUID:yoga@icloud\n\
    SUMMARY:Yoga\nDTSTART;TZID=Europe/Amsterdam:20261116T070000\nDTEND;TZID=Europe/Amsterdam:20261116T080000\n\
    RRULE:FREQ=WEEKLY;BYDAY=MO,TH;COUNT=6\nBEGIN:VALARM\nACTION:DISPLAY\nTRIGGER:-PT10M\nEND:VALARM\nEND:VEVENT\nEND:VCALENDAR\n";

fn amsterdam(ev: &Event) -> NaiveDateTime {
    crate::recurrence::to_zone(ev.start, Some("Europe/Amsterdam".parse().unwrap()))
}

#[test]
fn repeating_events_sync_occurrence_by_occurrence() {
    let server = FakeDav::start();
    server.put("home", "yoga.ics", SERIES);
    let (mut laptop, mut phone) = devices(&server);
    assert_eq!(laptop.titled("Yoga").len(), 6);
    assert!(laptop.titled("Yoga").iter().all(|e| e.recurring_id.as_deref() == Some("/1234/calendars/home/yoga.ics")));

    // Move one occurrence an hour later on the laptop.
    let third = laptop.titled("Yoga")[2].id.clone();
    laptop.edit(&third, |e| {
        e.title = "Yoga (late)".into();
        e.start += chrono::Duration::hours(1);
        e.end += chrono::Duration::hours(1);
    });
    laptop.pump();
    let file = server.file("home", "yoga.ics").unwrap();
    assert!(file.contains("RECURRENCE-ID;TZID=Europe/Amsterdam:20261123T070000"), "{file}");
    assert!(file.contains("BEGIN:VALARM"), "the alarm survives");

    // Delete another one on the phone.
    phone.sync();
    assert_eq!(amsterdam(phone.one("Yoga (late)")).hour(), 8);
    let fourth = phone.titled("Yoga")[2].id.clone();
    phone.delete(&fourth);
    phone.pump();
    assert!(server.file("home", "yoga.ics").unwrap().contains("EXDATE;TZID=Europe/Amsterdam:20261126T070000"));

    laptop.sync();
    let mut yoga: Vec<&Event> = laptop.store.events.iter().filter(|e| e.title.starts_with("Yoga")).collect();
    yoga.sort_by_key(|e| e.start);
    let days: Vec<(u32, u32)> = yoga.iter().map(|e| (amsterdam(e).month(), amsterdam(e).day())).collect();
    assert_eq!(days, [(11, 16), (11, 19), (11, 23), (11, 30), (12, 3)], "the 26th is gone, the 23rd moved");
    assert_eq!(laptop.one("Yoga (late)").start, phone.one("Yoga (late)").start);

    // Delete the whole series from the laptop (as "Delete All Events" does).
    let occ = laptop.titled("Yoga")[0].clone();
    let series = occ.recurring_id.clone().unwrap();
    laptop.store.events.retain(|e| e.recurring_id.as_deref() != Some(&series));
    let cal = laptop.store.remote_of(&occ).unwrap();
    laptop.engine.series_deleted(&mut laptop.store, cal, &series);
    laptop.pump();
    assert!(server.files("home").is_empty());
    phone.sync();
    assert!(phone.store.events.iter().all(|e| !e.title.starts_with("Yoga")));
}

#[test]
fn editing_a_new_series_before_its_occurrences_come_back_keeps_the_edit() {
    let server = FakeDav::start();
    let (mut laptop, _) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    let id = format!("local:{}", uuid::Uuid::new_v4().simple());
    let mut ev = crate::model::tests::event(&id, &home, "Piano");
    ev.start = at(17, 18);
    ev.end = at(17, 19);
    let mut rule = Recurrence::new(Freq::Weekly);
    rule.end = RepeatEnd::Count(4);
    ev.recurrence = Some(rule);
    laptop.store.events.push(ev);
    laptop.engine.event_changed(&mut laptop.store, &id);

    // The create is confirmed, but the sync that would bring back its
    // occurrences hasn't run yet when the user renames it.
    let save = laptop.engine.outbox.remove(0);
    let msg = run(&mut laptop.conns, save);
    laptop.engine.handle(&mut laptop.store, msg, None, laptop.today);
    let later: Vec<Cmd> = laptop.engine.outbox.drain(..).collect();
    let series_id = laptop.one("Piano").id.clone();
    laptop.edit(&series_id, |e| e.title = "Piano lesson".into());
    laptop.pump();
    laptop.engine.outbox.extend(later);
    laptop.pump();

    assert!(server.files("home")[0].1.contains("SUMMARY:Piano lesson"), "{:?}", server.files("home"));
    assert_eq!(laptop.titled("Piano lesson").len(), 4, "{:?}", laptop.said);
    assert!(laptop.said.iter().all(|s| !s.contains("different repeating event")), "{:?}", laptop.said);
}

#[test]
fn a_series_made_here_comes_back_as_occurrences() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    let id = format!("local:{}", uuid::Uuid::new_v4().simple());
    let mut ev = crate::model::tests::event(&id, &home, "Piano");
    ev.start = at(17, 18);
    ev.end = at(17, 19);
    let mut rule = Recurrence::new(Freq::Weekly);
    rule.end = RepeatEnd::Count(4);
    ev.recurrence = Some(rule);
    laptop.store.events.push(ev);
    laptop.engine.event_changed(&mut laptop.store, &id);
    laptop.pump();

    let piano = laptop.titled("Piano");
    assert_eq!(piano.len(), 4, "occurrences replace the series copy: {piano:?}");
    assert!(piano.iter().all(|e| e.recurrence.is_none() && e.recurring_id.is_some()));
    assert_eq!(piano[0].start, at(17, 18));
    assert!(server.files("home")[0].1.contains("RRULE:FREQ=WEEKLY;COUNT=4"));
    phone.sync();
    assert_eq!(phone.titled("Piano").len(), 4);
}

#[test]
fn rules_from_other_apps_show_every_occurrence() {
    let server = FakeDav::start();
    server.put(
        "home",
        "club.ics",
        "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:x\nBEGIN:VEVENT\nUID:club\nSUMMARY:Book club\n\
         DTSTART;TZID=Europe/Amsterdam:20261110T200000\nDURATION:PT2H\nRRULE:FREQ=MONTHLY;BYDAY=2TU\nEND:VEVENT\nEND:VCALENDAR\n",
    );
    let (laptop, _) = devices(&server);
    let dates: Vec<(u32, u32)> = laptop.titled("Book club").iter().take(3).map(|e| (amsterdam(e).month(), amsterdam(e).day())).collect();
    assert_eq!(dates, [(11, 10), (12, 8), (1, 12)]);
}

#[test]
fn what_the_app_does_not_show_survives_an_edit() {
    let server = FakeDav::start();
    server.put(
        "home",
        "m.ics",
        "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:x\nBEGIN:VEVENT\nUID:m\nSUMMARY:Meeting\nORGANIZER:mailto:boss@x\n\
         ATTENDEE;CN=Me:mailto:me@x\nX-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-TITLE=HQ:geo:52.37,4.89\n\
         DTSTART:20261119T130000Z\nDTEND:20261119T140000Z\nBEGIN:VALARM\nTRIGGER:-PT5M\nACTION:AUDIO\nEND:VALARM\nEND:VEVENT\nEND:VCALENDAR\n",
    );
    let (mut laptop, _) = devices(&server);
    let id = laptop.one("Meeting").id.clone();
    laptop.edit(&id, |e| e.title = "Meeting (room change)".into());
    laptop.pump();
    let file = server.file("home", "m.ics").unwrap();
    for kept in ["ORGANIZER:mailto:boss@x", "ATTENDEE;CN=Me:mailto:me@x", "X-APPLE-STRUCTURED-LOCATION", "TRIGGER:-PT5M", "UID:m"] {
        assert!(file.contains(kept), "lost {kept}: {file}");
    }
    assert!(file.contains("SUMMARY:Meeting (room change)"));
}

#[test]
fn changes_made_offline_upload_later() {
    let server = FakeDav::start();
    let (mut laptop, mut phone) = devices(&server);
    let home = laptop.cal(&server.cal_url("home"));
    server.with(|s| s.down = true);
    let id = laptop.create(&home, "Written on a plane", at(19, 9), 60);
    laptop.pump();
    assert!(laptop.store.event(&id).unwrap().pending);
    assert_eq!(laptop.store.unsynced_count(), 1);

    server.with(|s| s.down = false);
    laptop.sync();
    assert_eq!(laptop.store.unsynced_count(), 0);
    phone.sync();
    assert_eq!(phone.titled("Written on a plane").len(), 1);
}

#[test]
fn disconnecting_keeps_local_calendars() {
    let server = FakeDav::start();
    server.put("home", "x.ics", &plain_ics("x", "Coffee", "20261118T100000"));
    let (mut laptop, _) = devices(&server);
    laptop.create("local:home", "Local only", at(18, 9), 30);
    laptop.engine.disconnect(&mut laptop.store, ACCOUNT);
    assert!(laptop.store.caldav.is_empty());
    assert!(laptop.store.calendars.iter().all(|c| !c.is_remote()));
    assert_eq!(laptop.store.calendars.len(), 2);
    assert!(laptop.titled("Coffee").is_empty());
    assert_eq!(laptop.titled("Local only").len(), 1);
    assert!(laptop.store.sync_state.is_empty());
    // A sync that was already out is ignored when it comes back.
    laptop.sync();
    assert!(laptop.titled("Coffee").is_empty());
}
