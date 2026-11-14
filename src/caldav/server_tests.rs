//! The same end-to-end scenarios against a real CalDAV server: a local one
//! (Radicale, Xandikos, …) or iCloud itself. Ignored by default.
//!
//! Each test makes its own temporary calendar (MKCALENDAR) in the
//! account's calendar home and deletes it afterwards, so it never touches
//! existing calendars.
//!
//! Against Radicale:
//!
//! ```text
//! pip install radicale
//! python -m radicale --storage-filesystem-folder=/tmp/radicale --auth-type=none --server-hosts=127.0.0.1:5232
//! CLEANCALENDAR_CALDAV_URL=http://127.0.0.1:5232/ CLEANCALENDAR_CALDAV_USER=tester CLEANCALENDAR_CALDAV_PASSWORD=x \
//!     cargo test caldav::server_tests -- --ignored --test-threads 1
//! ```
//!
//! Against iCloud (use an app-specific password; never commit it):
//!
//! ```text
//! CLEANCALENDAR_CALDAV_URL=https://caldav.icloud.com/ CLEANCALENDAR_CALDAV_USER=you@icloud.com \
//! CLEANCALENDAR_CALDAV_PASSWORD=xxxx-xxxx-xxxx-xxxx cargo test caldav::server_tests -- --ignored --test-threads 1
//! ```

use chrono::{Local, NaiveDateTime, Timelike};

use super::tests::Device;
use super::*;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} (see src/caldav/server_tests.rs)"))
}

fn login() -> Login {
    Login {
        server: env("CLEANCALENDAR_CALDAV_URL"),
        username: env("CLEANCALENDAR_CALDAV_USER"),
        password: env("CLEANCALENDAR_CALDAV_PASSWORD"),
    }
}

/// A calendar made for one test, deleted when it ends.
struct TempCalendar {
    client: Client,
    url: Url,
}

impl TempCalendar {
    fn create() -> Self {
        let mut client = Client::new(login());
        let home = client.home().expect("discovery");
        let name = format!("cleancalendar-test-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let url = home.join(&format!("{name}/")).unwrap();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop>
<d:displayname>{name}</d:displayname>
<c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
</d:prop></d:set></c:mkcalendar>"#
        );
        let (_, resp) = client.send("MKCALENDAR", &url, &[], Some(("application/xml; charset=utf-8", body))).unwrap();
        Client::check(resp).expect("MKCALENDAR");
        TempCalendar { client, url }
    }

    fn files(&self) -> Vec<String> {
        let (rs, _) = self
            .client
            .report(&self.url, "1", xml::calendar_query("20000101T000000Z", "21000101T000000Z"))
            .expect("query");
        rs.into_iter().filter_map(|r| r.prop(xml::CALDAV, "calendar-data").map(|d| d.text.clone())).collect()
    }

    /// Writes an event as another client would.
    fn put(&self, file: &str, ics: &str) {
        let url = self.url.join(file).unwrap();
        let cal = ical::parse(ics).unwrap();
        self.client.put(&url, &cal, ("If-None-Match", "*")).expect("put");
    }
}

impl Drop for TempCalendar {
    fn drop(&mut self) {
        match self.client.send("DELETE", &self.url, &[], None) {
            Ok((_, r)) if r.status().is_success() => {}
            other => eprintln!("could not delete test calendar {}: {other:?}", self.url),
        }
    }
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn at(days: i64, hour: u32) -> NaiveDateTime {
    (today() + Duration::days(days)).and_hms_opt(hour, 0, 0).unwrap()
}

#[test]
#[ignore = "needs a CalDAV server; see src/caldav/server_tests.rs"]
fn server_sign_in_finds_the_test_calendar() {
    let cal = TempCalendar::create();
    let d = Device::sign_in("laptop", login(), today());
    let local = d.cal(cal.url.as_str());
    assert!(d.store.calendar(&local).is_some(), "calendars: {:?}", d.store.calendars.iter().map(|c| &c.id).collect::<Vec<_>>());
}

#[test]
#[ignore = "needs a CalDAV server; see src/caldav/server_tests.rs"]
fn server_create_edit_delete_across_two_devices() {
    let cal = TempCalendar::create();
    let mut laptop = Device::sign_in("laptop", login(), today());
    let mut phone = Device::sign_in("phone", login(), today());
    let local = laptop.cal(cal.url.as_str());

    laptop.create(&local, "CC test: lunch", at(1, 12), 60);
    laptop.pump();
    assert!(!laptop.one("CC test: lunch").pending, "{:?}", laptop.said);
    assert_eq!(cal.files().len(), 1);

    phone.sync();
    let id = phone.one("CC test: lunch").id.clone();
    phone.edit(&id, |e| e.location = "Canteen".into());
    phone.pump();
    laptop.edit(&id, |e| e.title = "CC test: long lunch".into());
    laptop.pump();
    let merged = laptop.one("CC test: long lunch");
    assert_eq!(merged.location, "Canteen", "{:?}", laptop.said);

    phone.sync();
    assert_eq!(phone.one("CC test: long lunch").location, "Canteen");
    phone.delete(&id);
    phone.pump();
    laptop.sync();
    assert!(laptop.titled("CC test: long lunch").is_empty());
    assert!(cal.files().is_empty());
}

#[test]
#[ignore = "needs a CalDAV server; see src/caldav/server_tests.rs"]
fn server_repeating_event_overrides_and_exceptions() {
    let cal = TempCalendar::create();
    let start = at(2, 7);
    cal.put(
        "cc-series.ics",
        &format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//CleanCalendar tests//EN\r\nBEGIN:VEVENT\r\nUID:cc-series\r\nSUMMARY:CC test: run\r\n\
             DTSTART:{}\r\nDURATION:PT1H\r\nRRULE:FREQ=DAILY;COUNT=4\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            start.format("%Y%m%dT%H%M%S")
        ),
    );
    let mut laptop = Device::sign_in("laptop", login(), today());
    let mut phone = Device::sign_in("phone", login(), today());
    assert_eq!(laptop.titled("CC test: run").len(), 4);

    let second = laptop.titled("CC test: run")[1].id.clone();
    laptop.edit(&second, |e| e.title = "CC test: long run".into());
    laptop.pump();
    let third = laptop.titled("CC test: run")[1].id.clone();
    laptop.delete(&third);
    laptop.pump();

    phone.sync();
    assert_eq!(phone.titled("CC test: run").len(), 2, "{:?}", phone.said);
    assert_eq!(phone.one("CC test: long run").start.hour(), 7);
    let files = cal.files();
    assert!(files[0].contains("RECURRENCE-ID") && files[0].contains("EXDATE"), "{}", files[0]);
}
