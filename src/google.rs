//! Google Calendar integration: OAuth 2.0 (installed-app loopback flow with
//! PKCE), the Calendar v3 REST API, and a background worker thread so the UI
//! never blocks on the network.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use url::Url;

use crate::model::Event;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/calendar/v3";
const SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const LOGIN_TIMEOUT: StdDuration = StdDuration::from_secs(300);

#[derive(Clone, Debug)]
pub struct Creds {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

#[derive(Clone, Debug)]
pub struct RemoteCalendar {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    pub read_only: bool,
    pub primary: bool,
}

/// Commands sent from the UI to the worker.
pub enum Cmd {
    Connect { client_id: String, client_secret: String },
    Sync { creds: Creds, from: NaiveDate, to: NaiveDate },
    Save { creds: Creds, google_calendar_id: String, event: Event },
    Delete { creds: Creds, google_calendar_id: String, remote_id: String },
}

/// Results sent from the worker back to the UI.
pub enum Msg {
    Connected { refresh_token: String },
    Synced { calendars: Vec<RemoteCalendar>, events: Vec<(String, Vec<Event>)> },
    Saved { local_id: String, remote_id: String },
    Deleted,
    Error(String),
}

pub struct Worker {
    tx: Sender<Cmd>,
    pub rx: Receiver<Msg>,
    pub busy: usize,
}

impl Worker {
    pub fn spawn(ctx: eframe::egui::Context) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("google-sync".into())
            .spawn(move || {
                let mut client: Option<Client> = None;
                for cmd in cmd_rx {
                    let msg = run(&mut client, cmd).unwrap_or_else(|e| Msg::Error(format!("{e:#}")));
                    if msg_tx.send(msg).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn sync thread");
        Worker { tx: cmd_tx, rx: msg_rx, busy: 0 }
    }

    pub fn send(&mut self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            self.busy += 1;
        }
    }
}

fn client_for(client: &mut Option<Client>, creds: Creds) -> &mut Client {
    let stale = client.as_ref().map_or(true, |c| c.creds.refresh_token != creds.refresh_token);
    if stale {
        *client = Some(Client::new(creds));
    }
    client.as_mut().unwrap()
}

fn run(client: &mut Option<Client>, cmd: Cmd) -> Result<Msg> {
    match cmd {
        Cmd::Connect { client_id, client_secret } => {
            let refresh_token = authorize(&client_id, &client_secret)?;
            Ok(Msg::Connected { refresh_token })
        }
        Cmd::Sync { creds, from, to } => {
            let c = client_for(client, creds);
            let calendars = c.list_calendars()?;
            let mut events = Vec::new();
            for cal in &calendars {
                events.push((cal.id.clone(), c.list_events(&cal.id, from, to)?));
            }
            Ok(Msg::Synced { calendars, events })
        }
        Cmd::Save { creds, google_calendar_id, event } => {
            let remote_id = client_for(client, creds).save_event(&google_calendar_id, &event)?;
            Ok(Msg::Saved { local_id: event.id, remote_id })
        }
        Cmd::Delete { creds, google_calendar_id, remote_id } => {
            client_for(client, creds).delete_event(&google_calendar_id, &remote_id)?;
            Ok(Msg::Deleted)
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth

/// Runs the browser sign-in flow and returns a refresh token.
fn authorize(client_id: &str, client_secret: &str) -> Result<String> {
    if client_id.trim().is_empty() {
        bail!("Enter a Google OAuth client ID first");
    }
    let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback port")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());

    let verifier = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().simple().to_string();

    let mut url = Url::parse(AUTH_URL)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id.trim())
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    open::that(url.as_str()).context("opening the browser")?;

    let code = wait_for_code(&listener, &state)?;

    let resp: Value = reqwest::blocking::Client::new()
        .post(TOKEN_URL)
        .form(&[
            ("code", code.as_str()),
            ("client_id", client_id.trim()),
            ("client_secret", client_secret.trim()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ])
        .send()?
        .json()?;
    if let Some(err) = resp.get("error") {
        bail!("Google rejected the sign-in: {err} {}", resp.get("error_description").unwrap_or(&Value::Null));
    }
    resp["refresh_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Google did not return a refresh token"))
}

/// Waits for the browser to hit the loopback redirect and returns the auth code.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    bail!("Timed out waiting for Google sign-in");
                }
                std::thread::sleep(StdDuration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(StdDuration::from_secs(5)))?;
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        let url = Url::parse(&format!("http://127.0.0.1{path}"))?;
        let param = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());

        let (result, body) = if let Some(err) = param("error") {
            (Some(Err(anyhow!("Sign-in was cancelled ({err})"))), "Sign-in was cancelled. You can close this tab.")
        } else if let Some(code) = param("code") {
            if param("state").as_deref() != Some(state) {
                (Some(Err(anyhow!("OAuth state mismatch"))), "Sign-in failed. You can close this tab.")
            } else {
                (Some(Ok(code)), "CleanCalendar is connected to Google. You can close this tab.")
            }
        } else {
            (None, "Not found")
        };
        let html = format!(
            "<html><body style=\"font-family:-apple-system,Segoe UI,sans-serif;text-align:center;padding-top:80px\"><h2>{body}</h2></body></html>"
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        if let Some(result) = result {
            return result;
        }
    }
}

// ---------------------------------------------------------------------------
// REST client

struct Client {
    http: reqwest::blocking::Client,
    creds: Creds,
    access: Option<(String, Instant)>,
}

impl Client {
    fn new(creds: Creds) -> Self {
        Client {
            http: reqwest::blocking::Client::builder()
                .timeout(StdDuration::from_secs(30))
                .build()
                .expect("http client"),
            creds,
            access: None,
        }
    }

    fn token(&mut self) -> Result<String> {
        if let Some((tok, exp)) = &self.access {
            if Instant::now() < *exp {
                return Ok(tok.clone());
            }
        }
        let resp: Value = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.creds.client_id.trim()),
                ("client_secret", self.creds.client_secret.trim()),
                ("refresh_token", self.creds.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()?
            .json()?;
        let tok = resp["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("Could not refresh Google access: {}", resp))?
            .to_owned();
        let ttl = resp["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
        self.access = Some((tok.clone(), Instant::now() + StdDuration::from_secs(ttl)));
        Ok(tok)
    }

    fn url(segments: &[&str]) -> Url {
        let mut url = Url::parse(API).unwrap();
        url.path_segments_mut().unwrap().extend(segments);
        url
    }

    fn check(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("Google API error {status}: {body}")
        }
    }

    fn list_calendars(&mut self) -> Result<Vec<RemoteCalendar>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            id: String,
            summary: Option<String>,
            summary_override: Option<String>,
            background_color: Option<String>,
            access_role: Option<String>,
            #[serde(default)]
            primary: bool,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            #[serde(default)]
            items: Vec<Entry>,
            next_page_token: Option<String>,
        }

        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = Self::url(&["users", "me", "calendarList"]);
            if let Some(t) = &page_token {
                url.query_pairs_mut().append_pair("pageToken", t);
            }
            let page: Page = Self::check(self.http.get(url).bearer_auth(self.token()?).send()?)?.json()?;
            for e in page.items {
                let role = e.access_role.unwrap_or_default();
                if role == "freeBusyReader" {
                    continue;
                }
                out.push(RemoteCalendar {
                    name: e.summary_override.or(e.summary).unwrap_or_else(|| e.id.clone()),
                    color: e.background_color.as_deref().and_then(parse_hex).unwrap_or([0x00, 0x7A, 0xFF]),
                    read_only: !(role == "owner" || role == "writer"),
                    primary: e.primary,
                    id: e.id,
                });
            }
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        out.sort_by_key(|c| (!c.primary, c.read_only, c.name.to_lowercase()));
        Ok(out)
    }

    fn list_events(&mut self, cal_id: &str, from: NaiveDate, to: NaiveDate) -> Result<Vec<Event>> {
        let time_min = to_rfc3339(from.and_hms_opt(0, 0, 0).unwrap());
        let time_max = to_rfc3339(to.and_hms_opt(0, 0, 0).unwrap());
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = Self::url(&["calendars", cal_id, "events"]);
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("singleEvents", "true")
                    .append_pair("maxResults", "2500")
                    .append_pair("timeMin", &time_min)
                    .append_pair("timeMax", &time_max);
                if let Some(t) = &page_token {
                    q.append_pair("pageToken", t);
                }
            }
            let page: Value = Self::check(self.http.get(url).bearer_auth(self.token()?).send()?)?.json()?;
            if let Some(items) = page["items"].as_array() {
                out.extend(items.iter().filter_map(|item| event_from_json(cal_id, item)));
            }
            page_token = page["nextPageToken"].as_str().map(str::to_owned);
            if page_token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Creates or updates the event and returns its Google id.
    fn save_event(&mut self, cal_id: &str, ev: &Event) -> Result<String> {
        let body = event_to_json(ev);
        let token = self.token()?;
        let req = match &ev.remote_id {
            Some(rid) => self.http.patch(Self::url(&["calendars", cal_id, "events", rid])),
            None => self.http.post(Self::url(&["calendars", cal_id, "events"])),
        };
        let resp: Value = Self::check(req.bearer_auth(token).json(&body).send()?)?.json()?;
        resp["id"].as_str().map(str::to_owned).ok_or_else(|| anyhow!("Google returned no event id"))
    }

    fn delete_event(&mut self, cal_id: &str, remote_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(Self::url(&["calendars", cal_id, "events", remote_id]))
            .bearer_auth(self.token()?)
            .send()?;
        // Already gone is fine.
        if resp.status().as_u16() == 404 || resp.status().as_u16() == 410 {
            return Ok(());
        }
        Self::check(resp)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Conversions

/// Local event id for a Google event, unique per calendar.
pub fn local_event_id(google_calendar_id: &str, remote_id: &str) -> String {
    format!("google:{google_calendar_id}:{remote_id}")
}

pub fn local_calendar_id(google_calendar_id: &str) -> String {
    format!("google:{google_calendar_id}")
}

fn event_from_json(cal_id: &str, v: &Value) -> Option<Event> {
    if v["status"].as_str() == Some("cancelled") {
        return None;
    }
    let remote_id = v["id"].as_str()?.to_owned();
    let (start, all_day) = parse_when(&v["start"])?;
    let (end, _) = parse_when(&v["end"]).unwrap_or((start, all_day));
    Some(Event {
        id: local_event_id(cal_id, &remote_id),
        calendar_id: local_calendar_id(cal_id),
        title: v["summary"].as_str().unwrap_or("(No title)").to_owned(),
        start,
        end,
        all_day,
        location: v["location"].as_str().unwrap_or_default().to_owned(),
        notes: v["description"].as_str().unwrap_or_default().to_owned(),
        remote_id: Some(remote_id),
    })
}

fn parse_when(v: &Value) -> Option<(NaiveDateTime, bool)> {
    if let Some(dt) = v["dateTime"].as_str() {
        let parsed = DateTime::parse_from_rfc3339(dt).ok()?;
        return Some((parsed.with_timezone(&Local).naive_local(), false));
    }
    let d = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    Some((d.and_hms_opt(0, 0, 0)?, true))
}

fn event_to_json(ev: &Event) -> Value {
    let when = |t: NaiveDateTime| {
        if ev.all_day {
            json!({ "date": t.date().format("%Y-%m-%d").to_string() })
        } else {
            json!({ "dateTime": to_rfc3339(t) })
        }
    };
    json!({
        "summary": ev.title,
        "location": ev.location,
        "description": ev.notes,
        "start": when(ev.start),
        "end": when(ev.end),
    })
}

fn to_rfc3339(t: NaiveDateTime) -> String {
    Local
        .from_local_datetime(&t)
        .earliest()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| t.and_utc().to_rfc3339())
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_day_and_timed_events() {
        let all_day = json!({"id": "x", "summary": "Trip",
            "start": {"date": "2026-11-11"}, "end": {"date": "2026-11-14"}});
        let ev = event_from_json("me@example.com", &all_day).unwrap();
        assert!(ev.all_day);
        assert_eq!(ev.calendar_id, "google:me@example.com");
        assert_eq!(event_to_json(&ev)["end"]["date"], "2026-11-14");

        let timed = json!({"id": "y", "start": {"dateTime": "2026-11-14T18:00:00Z"},
            "end": {"dateTime": "2026-11-14T19:00:00Z"}});
        let ev = event_from_json("c", &timed).unwrap();
        assert!(!ev.all_day);
        assert_eq!(ev.title, "(No title)");
        assert_eq!(ev.end - ev.start, chrono::Duration::hours(1));

        let cancelled = json!({"id": "z", "status": "cancelled"});
        assert!(event_from_json("c", &cancelled).is_none());
    }

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#9fe1e7"), Some([0x9f, 0xe1, 0xe7]));
        assert_eq!(parse_hex("nope"), None);
    }
}
