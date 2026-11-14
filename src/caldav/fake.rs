//! An in-memory CalDAV server for tests, shaped like iCloud:
//!
//! - Discovery starts on a "front" host, which names the principal and
//!   points to the calendar home on a second "partition" host (an absolute
//!   URL, as iCloud's `pNN-caldav.icloud.com` does). `/.well-known/caldav`
//!   redirects.
//! - HTTP Basic auth; anything else gets 401.
//! - The home holds calendars, a read-only one, a Reminders list (VTODO
//!   only) and a scheduling inbox, which a client must skip.
//! - Answers use default namespaces and 404 propstats, as iCloud's do.
//! - ETags, `If-Match`/`If-None-Match` (412), sync tokens (RFC 6578) that
//!   can be expired, `calendar-query` and `calendar-multiget`.
//! - Optionally no ETag on PUT responses (iCloud often leaves it out).
//!
//! Tests change the server directly to play "another device".

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use super::xml::{self, escape, CALDAV, DAV};
use super::Login;

pub const USER: &str = "tester@icloud.com";
pub const PASSWORD: &str = "abcd-efgh-ijkl-mnop";
const PRINCIPAL: &str = "/1234/principal/";
const HOME: &str = "/1234/calendars/";
const TOKEN_PREFIX: &str = "https://fake.invalid/sync/";

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Req {
    /// "front" or "partition".
    pub host: &'static str,
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub ics: String,
    pub etag: String,
}

#[derive(Clone, Debug)]
pub struct Cal {
    /// Its name in the home: "home" → /1234/calendars/home/.
    pub name: String,
    pub display: String,
    pub color: String,
    pub order: u32,
    pub read_only: bool,
    /// Supported components: ["VEVENT"], ["VTODO"], …
    pub components: Vec<&'static str>,
    /// By file name.
    pub items: BTreeMap<String, Item>,
    /// (sequence, file name), one entry per change.
    pub changes: Vec<(u64, String)>,
}

impl Cal {
    fn new(name: &str, display: &str, color: &str, order: u32) -> Self {
        Cal {
            name: name.into(),
            display: display.into(),
            color: color.into(),
            order,
            read_only: false,
            components: vec!["VEVENT"],
            items: BTreeMap::new(),
            changes: Vec::new(),
        }
    }

    fn path(&self) -> String {
        format!("{HOME}{}/", self.name)
    }
}

pub struct State {
    pub calendars: Vec<Cal>,
    /// Bumped on every change, anywhere.
    pub seq: u64,
    /// Tokens from before this are refused (403 valid-sync-token).
    pub tokens_valid_from: u64,
    /// Whether PUT answers carry the new ETag.
    pub etag_on_put: bool,
    /// Answers the next request matching (method, path suffix) with this
    /// status instead (e.g. a 503 for one upload).
    pub fail_next: Vec<(String, String, u16)>,
    /// Refuse everything (the server is "down"): connections are closed.
    pub down: bool,
    pub log: Vec<Req>,
    partition: String,
}

impl State {
    fn cal(&self, path: &str) -> Option<usize> {
        self.calendars.iter().position(|c| c.path() == path)
    }

    fn token(&self) -> String {
        format!("{TOKEN_PREFIX}{}", self.seq)
    }

    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Stores (or with `None` deletes) a file, recording the change.
    fn write(&mut self, ci: usize, file: &str, ics: Option<String>) -> Option<String> {
        let seq = self.bump();
        let cal = &mut self.calendars[ci];
        cal.changes.push((seq, file.to_owned()));
        match ics {
            Some(ics) => {
                let etag = format!("\"{}-{seq}\"", file.trim_end_matches(".ics"));
                cal.items.insert(file.to_owned(), Item { ics, etag: etag.clone() });
                Some(etag)
            }
            None => {
                cal.items.remove(file);
                None
            }
        }
    }
}

pub struct FakeDav {
    pub front: String,
    pub partition: String,
    state: Arc<Mutex<State>>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn resp(status: u16, body: impl Into<String>) -> Resp {
    Resp { status, headers: vec![("Content-Type".into(), "application/xml; charset=utf-8".into())], body: body.into() }
}

fn empty(status: u16) -> Resp {
    Resp { status, headers: Vec::new(), body: String::new() }
}

fn multistatus(responses: &[String], token: Option<&str>) -> Resp {
    let token = token.map(|t| format!("<sync-token>{}</sync-token>", escape(t))).unwrap_or_default();
    resp(
        207,
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<multistatus xmlns=\"DAV:\">{}{token}</multistatus>", responses.join("")),
    )
}

fn propstat(href: &str, props: &str) -> String {
    format!(
        "<response><href>{}</href><propstat><prop>{props}</prop><status>HTTP/1.1 200 OK</status></propstat>\
         <propstat><prop><getctag xmlns=\"http://calendarserver.org/ns/\"/></prop><status>HTTP/1.1 404 Not Found</status></propstat></response>",
        escape(href)
    )
}

fn item_response(href: &str, item: &Item, data: bool) -> String {
    let data = if data {
        format!("<calendar-data xmlns=\"urn:ietf:params:xml:ns:caldav\">{}</calendar-data>", escape(&item.ics))
    } else {
        String::new()
    };
    propstat(href, &format!("<getetag>{}</getetag>{data}", escape(&item.etag)))
}

fn not_found(href: &str) -> String {
    format!("<response><href>{}</href><status>HTTP/1.1 404 Not Found</status></response>", escape(href))
}

fn read_request(stream: &mut TcpStream, host: &'static str) -> Option<Req> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    Some(Req { host, method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

impl FakeDav {
    /// A server with "Home" and "Work" calendars, a read-only "Holidays",
    /// and a "Reminders" list.
    pub fn start() -> Self {
        let front_l = TcpListener::bind("127.0.0.1:0").unwrap();
        let part_l = TcpListener::bind("127.0.0.1:0").unwrap();
        let front = format!("http://{}", front_l.local_addr().unwrap());
        let partition = format!("http://{}", part_l.local_addr().unwrap());
        let mut holidays = Cal::new("holidays", "US Holidays", "#CC73E1FF", 3);
        holidays.read_only = true;
        let mut reminders = Cal::new("tasks", "Reminders", "#FF9500FF", 0);
        reminders.components = vec!["VTODO"];
        let state = Arc::new(Mutex::new(State {
            calendars: vec![
                Cal::new("work", "Work", "#1BADF8FF", 2),
                Cal::new("home", "Home", "#FF2968FF", 1),
                holidays,
                reminders,
            ],
            seq: 1,
            tokens_valid_from: 0,
            etag_on_put: true,
            fail_next: Vec::new(),
            down: false,
            log: Vec::new(),
            partition: partition.clone(),
        }));
        for (listener, host) in [(front_l, "front"), (part_l, "partition")] {
            let state = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let state = state.clone();
                    std::thread::spawn(move || {
                        let Some(req) = read_request(&mut stream, host) else { return };
                        let r = {
                            let mut s = state.lock().unwrap();
                            if s.down {
                                return; // drops the connection
                            }
                            s.log.push(req.clone());
                            handle(&mut s, &req)
                        };
                        let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
                        for (k, v) in &r.headers {
                            out += &format!("{k}: {v}\r\n");
                        }
                        out += "\r\n";
                        out += &r.body;
                        let _ = stream.write_all(out.as_bytes());
                    });
                }
            });
        }
        FakeDav { front, partition, state }
    }

    pub fn login(&self) -> Login {
        Login { server: self.front.clone(), username: USER.into(), password: PASSWORD.into() }
    }

    /// A calendar's URL, as the client names it.
    pub fn cal_url(&self, name: &str) -> String {
        format!("{}{HOME}{name}/", self.partition)
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// Stores a file as another device would; returns its ETag.
    pub fn put(&self, cal: &str, file: &str, ics: &str) -> String {
        self.with(|s| {
            let ci = s.cal(&format!("{HOME}{cal}/")).expect("calendar");
            s.write(ci, file, Some(ics.replace("\r\n", "\n").replace('\n', "\r\n"))).unwrap()
        })
    }

    pub fn remove(&self, cal: &str, file: &str) {
        self.with(|s| {
            let ci = s.cal(&format!("{HOME}{cal}/")).expect("calendar");
            s.write(ci, file, None);
        });
    }

    /// The files in a calendar: (name, data).
    pub fn files(&self, cal: &str) -> Vec<(String, String)> {
        self.with(|s| {
            let ci = s.cal(&format!("{HOME}{cal}/")).expect("calendar");
            s.calendars[ci].items.iter().map(|(k, v)| (k.clone(), v.ics.clone())).collect()
        })
    }

    pub fn file(&self, cal: &str, file: &str) -> Option<String> {
        self.files(cal).into_iter().find(|(f, _)| f == file).map(|(_, d)| d)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.with(|s| s.log.clone())
    }

    pub fn clear_log(&self) {
        self.with(|s| s.log.clear());
    }

    pub fn expire_tokens(&self) {
        self.with(|s| s.tokens_valid_from = s.seq + 1);
    }
}

fn authorized(req: &Req) -> bool {
    let expected = format!("Basic {}", STANDARD.encode(format!("{USER}:{PASSWORD}")));
    req.header("authorization") == Some(expected.as_str())
}

fn handle(s: &mut State, req: &Req) -> Resp {
    if let Some(i) = s.fail_next.iter().position(|(m, p, _)| *m == req.method && req.path.ends_with(p.as_str())) {
        let (_, _, status) = s.fail_next.remove(i);
        return empty(status);
    }
    if req.path == "/.well-known/caldav" {
        return Resp { status: 301, headers: vec![("Location".into(), "/".into())], body: String::new() };
    }
    if !authorized(req) {
        return Resp {
            status: 401,
            headers: vec![("WWW-Authenticate".into(), "Basic realm=\"test\"".into())],
            body: String::new(),
        };
    }
    let depth = req.header("depth").unwrap_or("0");
    match (req.method.as_str(), req.path.as_str()) {
        ("PROPFIND", "/") if req.host == "front" => multistatus(
            &[propstat("/", &format!("<current-user-principal><href>{PRINCIPAL}</href></current-user-principal>"))],
            None,
        ),
        ("PROPFIND", PRINCIPAL) => {
            // An absolute URL on another host, the way iCloud does it.
            let home = format!("{}{HOME}", s.partition);
            multistatus(
                &[propstat(
                    PRINCIPAL,
                    &format!("<calendar-home-set xmlns=\"urn:ietf:params:xml:ns:caldav\"><href xmlns=\"DAV:\">{home}</href></calendar-home-set>"),
                )],
                None,
            )
        }
        ("PROPFIND", HOME) if req.host == "partition" => {
            let mut out = vec![propstat(HOME, "<resourcetype><collection/></resourcetype>")];
            if depth != "0" {
                for c in &s.calendars {
                    let comps: String = c.components.iter().map(|n| format!("<C:comp name=\"{n}\"/>")).collect();
                    let privileges = if c.read_only {
                        "<privilege><read/></privilege>"
                    } else {
                        "<privilege><read/></privilege><privilege><write/></privilege>"
                    };
                    out.push(propstat(
                        &c.path(),
                        &format!(
                            "<resourcetype><collection/><calendar xmlns=\"urn:ietf:params:xml:ns:caldav\"/></resourcetype>\
                             <displayname>{}</displayname>\
                             <calendar-color xmlns=\"http://apple.com/ns/ical/\">{}</calendar-color>\
                             <calendar-order xmlns=\"http://apple.com/ns/ical/\">{}</calendar-order>\
                             <supported-calendar-component-set xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns=\"urn:ietf:params:xml:ns:caldav\">{comps}</supported-calendar-component-set>\
                             <current-user-privilege-set>{privileges}</current-user-privilege-set>",
                            escape(&c.display),
                            c.color,
                            c.order
                        ),
                    ));
                }
                out.push(propstat(
                    &format!("{HOME}inbox/"),
                    "<resourcetype><collection/><schedule-inbox xmlns=\"urn:ietf:params:xml:ns:caldav\"/></resourcetype>",
                ));
            }
            multistatus(&out, None)
        }
        ("PROPFIND", path) if req.host == "partition" => match s.cal(path) {
            Some(_) => multistatus(&[propstat(path, &format!("<sync-token>{}</sync-token>", s.token()))], None),
            None => empty(404),
        },
        ("REPORT", path) if req.host == "partition" => {
            let Some(ci) = s.cal(path) else { return empty(404) };
            report(s, ci, req)
        }
        (method, path) if req.host == "partition" => {
            let Some((dir, file)) = path.rsplit_once('/') else { return empty(404) };
            let Some(ci) = s.cal(&format!("{dir}/")) else { return empty(404) };
            item(s, ci, file, method, req)
        }
        _ => empty(404),
    }
}

fn report(s: &mut State, ci: usize, req: &Req) -> Resp {
    let Ok(body) = xml::parse(&req.body) else { return empty(400) };
    let cal = &s.calendars[ci];
    let path = cal.path();
    let href = |f: &str| format!("{path}{f}");
    if body.is(CALDAV, "calendar-query") {
        let out: Vec<String> = cal.items.iter().map(|(f, it)| item_response(&href(f), it, true)).collect();
        return multistatus(&out, None);
    }
    if body.is(CALDAV, "calendar-multiget") {
        let out: Vec<String> = body
            .children_named(DAV, "href")
            .map(|h| {
                let h = h.text.trim();
                let file = h.rsplit('/').next().unwrap_or(h);
                match cal.items.get(file) {
                    Some(it) => item_response(h, it, true),
                    None => not_found(h),
                }
            })
            .collect();
        return multistatus(&out, None);
    }
    if body.is(DAV, "sync-collection") {
        let token = body.child(DAV, "sync-token").map(|t| t.text.trim().to_owned()).unwrap_or_default();
        let since = token.strip_prefix(TOKEN_PREFIX).and_then(|n| n.parse::<u64>().ok());
        let Some(since) = since.filter(|n| *n >= s.tokens_valid_from && *n <= s.seq) else {
            return resp(403, "<?xml version=\"1.0\"?><error xmlns=\"DAV:\"><valid-sync-token/></error>");
        };
        let mut files: Vec<&String> = cal.changes.iter().filter(|(n, _)| *n > since).map(|(_, f)| f).collect();
        files.sort();
        files.dedup();
        let out: Vec<String> = files
            .into_iter()
            .map(|f| match cal.items.get(f) {
                Some(it) => item_response(&href(f), it, false),
                None => not_found(&href(f)),
            })
            .collect();
        return multistatus(&out, Some(&s.token()));
    }
    empty(400)
}

fn item(s: &mut State, ci: usize, file: &str, method: &str, req: &Req) -> Resp {
    let current = s.calendars[ci].items.get(file).cloned();
    let if_match = req.header("if-match");
    let precondition_failed = match (if_match, &current) {
        (Some("*"), None) => true,
        (Some(e), Some(it)) if e != "*" && e != it.etag => true,
        (Some(e), None) if e != "*" => true,
        _ => false,
    } || (req.header("if-none-match") == Some("*") && current.is_some());
    match method {
        "GET" => match current {
            Some(it) => Resp {
                status: 200,
                headers: vec![("Content-Type".into(), "text/calendar; charset=utf-8".into()), ("ETag".into(), it.etag)],
                body: it.ics,
            },
            None => empty(404),
        },
        "PUT" => {
            if s.calendars[ci].read_only {
                return resp(403, "<?xml version=\"1.0\"?><error xmlns=\"DAV:\"><need-privileges/></error>");
            }
            if precondition_failed {
                return empty(412);
            }
            if super::ical::parse(&req.body).ok().and_then(|c| super::ical::uid(&c)).is_none() {
                return resp(400, "<?xml version=\"1.0\"?><error xmlns=\"urn:ietf:params:xml:ns:caldav\"><valid-calendar-data/></error>");
            }
            let created = current.is_none();
            let etag = s.write(ci, file, Some(req.body.clone())).unwrap();
            let mut headers = Vec::new();
            if s.etag_on_put {
                headers.push(("ETag".into(), etag));
            }
            Resp { status: if created { 201 } else { 204 }, headers, body: String::new() }
        }
        "DELETE" => {
            if s.calendars[ci].read_only {
                return empty(403);
            }
            if current.is_none() {
                return empty(404);
            }
            if precondition_failed {
                return empty(412);
            }
            s.write(ci, file, None);
            empty(204)
        }
        _ => empty(405),
    }
}
