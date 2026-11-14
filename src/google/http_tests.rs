//! Tests for the Google HTTP client against a scripted local fake server.
//! They exercise real HTTP (requests, URLs, bodies, status codes) without
//! touching Google.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;

/// One request the fake server received.
#[derive(Clone, Debug)]
struct Req {
    method: String,
    /// Raw (still percent-encoded) path.
    path: String,
    query: Vec<(String, String)>,
    auth: Option<String>,
    body: String,
}

impl Req {
    fn q(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// A minimal HTTP/1.1 server answering every request with `handler`.
/// Each response closes the connection, which keeps parsing trivial.
struct FakeServer {
    base: String,
    log: Arc<Mutex<Vec<Req>>>,
}

impl FakeServer {
    fn start(handler: impl Fn(&Req) -> (u16, Value) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(req) = read_request(&mut stream) else { continue };
                log2.lock().unwrap().push(req.clone());
                let (status, body) = handler(&req);
                let body = if body.is_null() { String::new() } else { body.to_string() };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        FakeServer { base, log }
    }

    fn client(&self) -> Client {
        Client::with_endpoints(creds(), &format!("{}/calendar/v3", self.base), &format!("{}/token", self.base))
    }

    /// Requests received so far, excluding token refreshes.
    fn api_requests(&self) -> Vec<Req> {
        self.log.lock().unwrap().iter().filter(|r| r.path != "/token").cloned().collect()
    }

    fn token_requests(&self) -> Vec<Req> {
        self.log.lock().unwrap().iter().filter(|r| r.path == "/token").cloned().collect()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();

    let mut content_length = 0;
    let mut auth = None;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (name, value) = h.split_once(':')?;
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().ok()?,
            "authorization" => auth = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).ok()?;

    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    Some(Req { method, path: path.to_owned(), query, auth, body: String::from_utf8_lossy(&body).into_owned() })
}

fn creds() -> Creds {
    Creds { client_id: "cid".into(), client_secret: "secret".into(), refresh_token: "refresh".into() }
}

fn token_ok() -> (u16, Value) {
    (200, json!({"access_token": "at1", "expires_in": 3600}))
}

fn google_error(status: u16, message: &str) -> (u16, Value) {
    (status, json!({"error": {"code": status, "message": message}}))
}

fn item(id: &str, title: &str) -> Value {
    json!({"id": id, "summary": title,
        "start": {"dateTime": "2026-11-14T09:00:00Z"}, "end": {"dateTime": "2026-11-14T10:00:00Z"}})
}

const CAL_EVENTS: &str = "/calendar/v3/calendars/primary/events";

fn window() -> Query<'static> {
    Query::Window(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(), NaiveDate::from_ymd_opt(2027, 12, 1).unwrap())
}

// ---- auth -----------------------------------------------------------------

#[test]
fn access_token_is_refreshed_once_and_sent_as_bearer() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        _ => (200, json!({"items": []})),
    });
    let mut c = server.client();
    c.list_calendars().unwrap();
    c.list_calendars().unwrap();

    let tokens = server.token_requests();
    assert_eq!(tokens.len(), 1, "cached between calls");
    assert!(tokens[0].body.contains("grant_type=refresh_token"));
    assert!(tokens[0].body.contains("refresh_token=refresh"));
    for r in server.api_requests() {
        assert_eq!(r.auth.as_deref(), Some("Bearer at1"));
    }
}

#[test]
fn failed_token_refresh_is_retryable() {
    let server = FakeServer::start(|_| (400, json!({"error": "invalid_grant"})));
    let err = server.client().list_calendars().unwrap_err();
    assert_eq!(classify(&err), Failure::Retry);
    assert!(err.to_string().contains("invalid_grant"));
}

// ---- calendar list --------------------------------------------------------

#[test]
fn calendar_list_pages_filters_and_sorts() {
    let server = FakeServer::start(|r| match (r.path.as_str(), r.q("pageToken")) {
        ("/token", _) => token_ok(),
        (_, None) => (
            200,
            json!({"items": [
                {"id": "holidays", "summary": "Holidays", "accessRole": "reader", "backgroundColor": "#16a765"},
                {"id": "busy", "summary": "Someone's busy times", "accessRole": "freeBusyReader"},
            ], "nextPageToken": "p2"}),
        ),
        (_, Some("p2")) => (
            200,
            json!({"items": [
                {"id": "me@example.com", "summary": "me@example.com", "summaryOverride": "Me",
                 "accessRole": "owner", "primary": true},
            ]}),
        ),
        _ => (404, Value::Null),
    });
    let cals = server.client().list_calendars().unwrap();
    let summary: Vec<(&str, &str, bool)> = cals.iter().map(|c| (c.id.as_str(), c.name.as_str(), c.read_only)).collect();
    assert_eq!(summary, [("me@example.com", "Me", false), ("holidays", "Holidays", true)]);
    assert_eq!(cals[1].color, [0x16, 0xa7, 0x65]);
}

// ---- events: full and incremental -----------------------------------------

#[test]
fn full_sync_follows_pages_and_keeps_the_final_sync_token() {
    let server = FakeServer::start(|r| match (r.path.as_str(), r.q("pageToken")) {
        ("/token", _) => token_ok(),
        (_, None) => (200, json!({"items": [item("a", "A"), item("b", "B")], "nextPageToken": "p2"})),
        (_, Some("p2")) => (200, json!({"items": [item("c", "C")], "nextSyncToken": "s1"})),
        _ => (404, Value::Null),
    });
    let ch = server.client().list_events("primary", window()).unwrap();
    assert!(ch.full);
    assert_eq!(ch.events.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["A", "B", "C"]);
    assert_eq!(ch.next_token.as_deref(), Some("s1"));

    let reqs = server.api_requests();
    assert_eq!(reqs.len(), 2);
    let first = &reqs[0];
    assert_eq!((first.method.as_str(), first.path.as_str()), ("GET", CAL_EVENTS));
    assert_eq!(first.q("singleEvents"), Some("true"));
    assert!(first.q("timeMin").unwrap().starts_with("2026-05-01T00:00:00"));
    assert!(first.q("timeMax").unwrap().starts_with("2027-12-01T00:00:00"));
    assert_eq!(first.q("syncToken"), None);
}

#[test]
fn incremental_sync_sends_only_the_token_and_reports_cancellations() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        _ => (
            200,
            json!({"items": [item("a", "Renamed"), {"id": "gone", "status": "cancelled"}], "nextSyncToken": "s2"}),
        ),
    });
    let ch = server.client().list_events("primary", Query::Changes("s1")).unwrap();
    assert!(!ch.full);
    assert_eq!(ch.events[0].title, "Renamed");
    assert_eq!(ch.cancelled, ["gone"]);
    assert_eq!(ch.next_token.as_deref(), Some("s2"));

    let r = &server.api_requests()[0];
    assert_eq!(r.q("syncToken"), Some("s1"));
    // Google rejects these together with a sync token.
    assert_eq!((r.q("timeMin"), r.q("timeMax")), (None, None));
}

#[test]
fn expired_sync_token_falls_back_to_a_full_sync() {
    let server = FakeServer::start(|r| match (r.path.as_str(), r.q("syncToken")) {
        ("/token", _) => token_ok(),
        ("/calendar/v3/users/me/calendarList", _) => {
            (200, json!({"items": [{"id": "primary", "summary": "Me", "accessRole": "owner", "primary": true}]}))
        }
        (_, Some(_)) => google_error(410, "Sync token is no longer valid, a full sync is required."),
        (_, None) => (200, json!({"items": [item("a", "A")], "nextSyncToken": "fresh"})),
    });
    let tokens = HashMap::from([("primary".to_owned(), "old".to_owned())]);
    let from = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
    let (cals, changes) = sync_all(&mut server.client(), from, from + chrono::Duration::days(400), &tokens).unwrap();

    assert_eq!(cals.len(), 1);
    assert_eq!(changes.len(), 1);
    assert!(changes[0].full);
    assert_eq!(changes[0].next_token.as_deref(), Some("fresh"));
    let event_reqs: Vec<_> = server.api_requests().into_iter().filter(|r| r.path == CAL_EVENTS).collect();
    assert_eq!(event_reqs.len(), 2);
    assert_eq!(event_reqs[0].q("syncToken"), Some("old"));
    assert!(event_reqs[1].q("timeMin").is_some());
}

#[test]
fn other_sync_errors_fail_the_whole_sync() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        "/calendar/v3/users/me/calendarList" => (200, json!({"items": [{"id": "primary", "accessRole": "owner"}]})),
        _ => google_error(503, "Backend Error"),
    });
    let from = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
    let err = sync_all(&mut server.client(), from, from, &HashMap::new()).err().expect("should fail");
    assert_eq!(classify(&err), Failure::Retry);
    assert!(err.to_string().contains("Backend Error"));
}

#[test]
fn calendar_ids_are_percent_encoded_in_paths() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        _ => (200, json!({"items": []})),
    });
    server.client().list_events("en.usa#holiday@group.v.calendar.google.com", window()).unwrap();
    assert_eq!(
        server.api_requests()[0].path,
        "/calendar/v3/calendars/en.usa%23holiday@group.v.calendar.google.com/events"
    );
}

// ---- writes ---------------------------------------------------------------

fn local_event(remote_id: Option<&str>) -> Event {
    let start = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap().and_hms_opt(9, 0, 0).unwrap();
    Event {
        id: "local:1".into(),
        calendar_id: "google:primary".into(),
        title: "Lunch".into(),
        start,
        end: start + chrono::Duration::hours(1),
        all_day: false,
        location: "Cafe".into(),
        notes: "bring cash".into(),
        remote_id: remote_id.map(str::to_owned),
        recurring_id: None,
        pending: true,
        rev: 3,
    }
}

#[test]
fn new_events_are_posted_and_existing_ones_patched() {
    let server = FakeServer::start(|r| match (r.path.as_str(), r.method.as_str()) {
        ("/token", _) => token_ok(),
        (_, "POST") => (200, json!({"id": "new1"})),
        (_, "PATCH") => (200, json!({"id": "e1"})),
        _ => (405, Value::Null),
    });
    let mut c = server.client();
    assert_eq!(c.save_event("primary", &local_event(None)).unwrap(), "new1");
    assert_eq!(c.save_event("primary", &local_event(Some("e1"))).unwrap(), "e1");

    let reqs = server.api_requests();
    assert_eq!((reqs[0].method.as_str(), reqs[0].path.as_str()), ("POST", CAL_EVENTS));
    assert_eq!((reqs[1].method.as_str(), reqs[1].path.as_str()), ("PATCH", "/calendar/v3/calendars/primary/events/e1"));
    let body = reqs[0].json();
    assert_eq!(body["summary"], "Lunch");
    assert_eq!(body["location"], "Cafe");
    assert_eq!(body["description"], "bring cash");
    assert!(body["start"]["dateTime"].as_str().unwrap().starts_with("2026-11-14T09:00:00"));
}

#[test]
fn all_day_events_are_sent_as_dates() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        _ => (200, json!({"id": "x"})),
    });
    let mut ev = local_event(None);
    ev.all_day = true;
    ev.start = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap().and_hms_opt(0, 0, 0).unwrap();
    ev.end = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap().and_hms_opt(0, 0, 0).unwrap();
    server.client().save_event("primary", &ev).unwrap();
    let body = server.api_requests()[0].json();
    assert_eq!(body["start"], json!({"date": "2026-11-11"}));
    assert_eq!(body["end"], json!({"date": "2026-11-14"}));
}

#[test]
fn deleting_an_already_deleted_event_succeeds() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        p if p.ends_with("/gone") => google_error(410, "Resource has been deleted"),
        p if p.ends_with("/missing") => google_error(404, "Not Found"),
        p if p.ends_with("/ok") => (204, Value::Null),
        _ => google_error(500, "Backend Error"),
    });
    let mut c = server.client();
    c.delete_event("primary", "gone").unwrap();
    c.delete_event("primary", "missing").unwrap();
    c.delete_event("primary", "ok").unwrap();
    let err = c.delete_event("primary", "broken").unwrap_err();
    assert_eq!(classify(&err), Failure::Retry);
    assert!(server.api_requests().iter().all(|r| r.method == "DELETE"));
}

// ---- worker command handling ------------------------------------------------

#[test]
fn worker_reports_save_results_with_revision_and_failure_kind() {
    let server = FakeServer::start(|r| match (r.path.as_str(), r.method.as_str()) {
        ("/token", _) => token_ok(),
        (_, "POST") => (200, json!({"id": "new1"})),
        (_, "PATCH") => google_error(404, "Not Found"),
        _ => (405, Value::Null),
    });
    let mut client = Some(server.client());
    let save = |ev: Event| Cmd::Save { creds: creds(), google_calendar_id: "primary".into(), event: ev };

    match run(&mut client, save(local_event(None))) {
        Msg::Saved { local_id, rev, remote_id, google_calendar_id } => {
            assert_eq!((local_id.as_str(), rev, remote_id.as_str()), ("local:1", 3, "new1"));
            assert_eq!(google_calendar_id, "primary");
        }
        _ => panic!("expected Saved"),
    }
    match run(&mut client, save(local_event(Some("deleted-elsewhere")))) {
        Msg::SaveFailed { kind, error, .. } => {
            assert_eq!(kind, Failure::Gone);
            assert!(error.contains("Not Found"), "{error}");
        }
        _ => panic!("expected SaveFailed"),
    }
}

#[test]
fn worker_turns_rejections_into_rejected_failures() {
    let server = FakeServer::start(|r| match r.path.as_str() {
        "/token" => token_ok(),
        _ => google_error(403, "You need to have writer access to this calendar."),
    });
    let mut client = Some(server.client());
    let cmd = Cmd::Save { creds: creds(), google_calendar_id: "primary".into(), event: local_event(Some("e1")) };
    match run(&mut client, cmd) {
        Msg::SaveFailed { kind, error, .. } => {
            assert_eq!(kind, Failure::Rejected);
            assert!(error.contains("writer access"), "{error}");
        }
        _ => panic!("expected SaveFailed"),
    }
}
