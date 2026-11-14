//! Google Calendar integration: OAuth 2.0 (installed-app loopback flow with
//! PKCE), the Calendar v3 REST API, and a background worker thread so the UI
//! never blocks on the network.

use std::collections::HashMap;
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
use crate::recurrence;

/// Where sign-in, token, and Calendar API requests go.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoints {
    pub auth: String,
    pub token: String,
    pub api: String,
}

/// Environment variable that points the app at a Google emulator.
pub const EMULATOR_ENV: &str = "CLEANCALENDAR_GOOGLE_URL";

impl Endpoints {
    pub fn google() -> Self {
        Endpoints {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            api: "https://www.googleapis.com/calendar/v3".into(),
        }
    }

    /// Everything under one base URL with Google's paths, the layout used
    /// by emulators such as `npx emulate` (e.g. `http://127.0.0.1:4002`).
    pub fn at(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Endpoints {
            auth: format!("{base}/o/oauth2/v2/auth"),
            token: format!("{base}/oauth2/token"),
            api: format!("{base}/calendar/v3"),
        }
    }

    /// Real Google, unless `CLEANCALENDAR_GOOGLE_URL` names an emulator.
    pub fn from_env() -> Self {
        match std::env::var(EMULATOR_ENV) {
            Ok(base) if !base.trim().is_empty() => Self::at(base.trim()),
            _ => Self::google(),
        }
    }
}
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
    Connect {
        client_id: String,
        client_secret: String,
    },
    /// Fetch the calendar list, then changes for each calendar: incremental
    /// when `tokens` has a sync token for it, otherwise everything in
    /// [`from`, `to`).
    Sync {
        creds: Creds,
        from: NaiveDate,
        to: NaiveDate,
        tokens: HashMap<String, String>,
    },
    Save {
        creds: Creds,
        google_calendar_id: String,
        event: Event,
    },
    Delete {
        creds: Creds,
        google_calendar_id: String,
        remote_id: String,
        /// Only delete this version; `None` deletes unconditionally.
        etag: Option<String>,
    },
}

/// What one calendar's sync returned.
pub struct CalendarChanges {
    pub calendar_id: String,
    /// `true`: `events` is the complete set for the sync window.
    /// `false`: `events` and `cancelled` are only what changed.
    pub full: bool,
    pub events: Vec<Event>,
    /// Remote ids of deleted events (or deleted recurring series).
    pub cancelled: Vec<String>,
    pub next_token: Option<String>,
}

/// How an upload failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Offline, rate-limited, or a server error: try again later.
    Retry,
    /// The event no longer exists on Google.
    Gone,
    /// Google refused the change; retrying won't help.
    Rejected,
}

/// Results sent from the worker back to the UI.
pub enum Msg {
    Connected { refresh_token: String },
    ConnectFailed(String),
    Synced { calendars: Vec<RemoteCalendar>, changes: Vec<CalendarChanges> },
    SyncFailed(String),
    Saved { local_id: String, google_calendar_id: String, rev: u64, remote_id: String, etag: Option<String> },
    SaveFailed { local_id: String, google_calendar_id: String, error: String, kind: Failure },
    /// The event changed on Google since we last saw it (or was deleted
    /// there: `server` is `None`). Carries Google's current copy.
    SaveConflict { local_id: String, server: Option<Event> },
    Deleted { remote_id: String },
    DeleteFailed { remote_id: String, google_calendar_id: String, error: String, kind: Failure },
    /// Someone changed the event after we decided to delete it; nothing
    /// was deleted. Carries Google's current copy (`None` if it's gone).
    DeleteConflict { remote_id: String, google_calendar_id: String, server: Option<Event> },
}

/// A non-success HTTP response from Google.
#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Google returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for HttpError {}

fn http_status(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpError>().map(|h| h.status)
}

fn classify(e: &anyhow::Error) -> Failure {
    match e.downcast_ref::<HttpError>() {
        // Network errors, timeouts, token refresh problems: transient.
        None => Failure::Retry,
        Some(h) => match h.status {
            404 | 410 => Failure::Gone,
            401 | 408 | 429 | 500..=599 => Failure::Retry,
            403 if h.message.to_lowercase().contains("rate limit") => Failure::Retry,
            _ => Failure::Rejected,
        },
    }
}

pub struct Worker {
    tx: Sender<Cmd>,
    pub rx: Receiver<Msg>,
    pub busy: usize,
}

impl Worker {
    pub fn spawn(ctx: eframe::egui::Context, endpoints: Endpoints) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("google-sync".into())
            .spawn(move || {
                let mut client: Option<Client> = None;
                let open_browser = |url: &str| open::that(url).context("opening the browser");
                for cmd in cmd_rx {
                    let msg = run(&mut client, &endpoints, &open_browser, cmd);
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

fn client_for<'a>(client: &'a mut Option<Client>, endpoints: &Endpoints, creds: Creds) -> &'a mut Client {
    let stale = client.as_ref().map_or(true, |c| c.creds.refresh_token != creds.refresh_token);
    if stale {
        *client = Some(Client::new(creds, endpoints));
    }
    client.as_mut().unwrap()
}

/// Executes one command; what the worker thread does for each message.
fn run(
    client: &mut Option<Client>,
    endpoints: &Endpoints,
    open_browser: &dyn Fn(&str) -> Result<()>,
    cmd: Cmd,
) -> Msg {
    let text = |e: anyhow::Error| format!("{e:#}");
    match cmd {
        Cmd::Connect { client_id, client_secret } => {
            match authorize(endpoints, &client_id, &client_secret, open_browser) {
                Ok(refresh_token) => Msg::Connected { refresh_token },
                Err(e) => Msg::ConnectFailed(text(e)),
            }
        }
        Cmd::Sync { creds, from, to, tokens } => {
            match sync_all(client_for(client, endpoints, creds), from, to, &tokens) {
                Ok((calendars, changes)) => Msg::Synced { calendars, changes },
                Err(e) => Msg::SyncFailed(text(e)),
            }
        }
        Cmd::Save { creds, google_calendar_id, event } => {
            let c = client_for(client, endpoints, creds);
            let failed = |e: anyhow::Error, local_id: String, google_calendar_id: String| Msg::SaveFailed {
                local_id,
                google_calendar_id,
                kind: classify(&e),
                error: text(e),
            };
            match c.save_event(&google_calendar_id, &event) {
                Ok((remote_id, etag)) => {
                    Msg::Saved { local_id: event.id, google_calendar_id, rev: event.rev, remote_id, etag }
                }
                // Changed on Google since we last saw it: fetch their copy.
                Err(e) if http_status(&e) == Some(412) => {
                    let rid = event.remote_id.clone().unwrap_or_default();
                    match c.get_event(&google_calendar_id, &rid) {
                        Ok(server) => Msg::SaveConflict { local_id: event.id, server },
                        Err(e) => failed(e, event.id, google_calendar_id),
                    }
                }
                // Updating an event that was deleted on Google.
                Err(e) if classify(&e) == Failure::Gone && event.remote_id.is_some() => {
                    Msg::SaveConflict { local_id: event.id, server: None }
                }
                Err(e) => failed(e, event.id, google_calendar_id),
            }
        }
        Cmd::Delete { creds, google_calendar_id, remote_id, etag } => {
            let c = client_for(client, endpoints, creds);
            match c.delete_event(&google_calendar_id, &remote_id, etag.as_deref()) {
                Ok(()) => Msg::Deleted { remote_id },
                // Changed on Google after we decided to delete it.
                Err(e) if http_status(&e) == Some(412) => match c.get_event(&google_calendar_id, &remote_id) {
                    Ok(server) => Msg::DeleteConflict { remote_id, google_calendar_id, server },
                    Err(e) => Msg::DeleteFailed { remote_id, google_calendar_id, kind: classify(&e), error: text(e) },
                },
                Err(e) => Msg::DeleteFailed { remote_id, google_calendar_id, kind: classify(&e), error: text(e) },
            }
        }
    }
}

fn sync_all(
    c: &mut Client,
    from: NaiveDate,
    to: NaiveDate,
    tokens: &HashMap<String, String>,
) -> Result<(Vec<RemoteCalendar>, Vec<CalendarChanges>)> {
    let calendars = c.list_calendars()?;
    let mut out = Vec::new();
    for cal in &calendars {
        let incremental = match tokens.get(&cal.id) {
            Some(token) => match c.list_events(&cal.id, Query::Changes(token)) {
                Ok(changes) => Some(changes),
                // The token expired: Google wants a full sync.
                Err(e) if http_status(&e) == Some(410) => None,
                Err(e) => return Err(e),
            },
            None => None,
        };
        let changes = match incremental {
            Some(changes) => changes,
            None => c.list_events(&cal.id, Query::Window(from, to))?,
        };
        out.push(changes);
    }
    Ok((calendars, out))
}

enum Query<'a> {
    Window(NaiveDate, NaiveDate),
    Changes(&'a str),
}

// ---------------------------------------------------------------------------
// OAuth

/// Runs the browser sign-in flow and returns a refresh token.
/// `open_browser` shows the sign-in page (tests drive it over HTTP instead).
fn authorize(
    endpoints: &Endpoints,
    client_id: &str,
    client_secret: &str,
    open_browser: &dyn Fn(&str) -> Result<()>,
) -> Result<String> {
    if client_id.trim().is_empty() {
        bail!("Enter a Google OAuth client ID first");
    }
    let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback port")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());

    let verifier = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().simple().to_string();

    let mut url = Url::parse(&endpoints.auth)?;
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
    open_browser(url.as_str())?;

    let code = wait_for_code(&listener, &state)?;

    let resp: Value = reqwest::blocking::Client::new()
        .post(&endpoints.token)
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
    /// Base URL of the Calendar API.
    api: String,
    token_url: String,
}

impl Client {
    fn new(creds: Creds, endpoints: &Endpoints) -> Self {
        Client {
            http: reqwest::blocking::Client::builder()
                .timeout(StdDuration::from_secs(30))
                .build()
                .expect("http client"),
            creds,
            access: None,
            api: endpoints.api.clone(),
            token_url: endpoints.token.clone(),
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
            .post(&self.token_url)
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

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = Url::parse(&self.api).expect("valid API base URL");
        url.path_segments_mut().expect("API base URL has a path").pop_if_empty().extend(segments);
        url
    }

    fn check(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let body = resp.text().unwrap_or_default();
        // Google errors look like {"error": {"code": 404, "message": "Not Found", ...}}
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(body);
        Err(HttpError { status, message }.into())
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
            let mut url = self.url(&["users", "me", "calendarList"]);
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

    /// Lists a calendar's events: everything in a date window (full sync),
    /// or what changed since a sync token (incremental sync). Recurring
    /// events are expanded into individual occurrences either way.
    fn list_events(&mut self, cal_id: &str, query: Query) -> Result<CalendarChanges> {
        let mut out = CalendarChanges {
            calendar_id: cal_id.to_owned(),
            full: matches!(query, Query::Window(..)),
            events: Vec::new(),
            cancelled: Vec::new(),
            next_token: None,
        };
        let mut page_token: Option<String> = None;
        loop {
            let mut url = self.url(&["calendars", cal_id, "events"]);
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("singleEvents", "true").append_pair("maxResults", "2500");
                match &query {
                    Query::Window(from, to) => {
                        q.append_pair("timeMin", &to_rfc3339(from.and_hms_opt(0, 0, 0).unwrap()))
                            .append_pair("timeMax", &to_rfc3339(to.and_hms_opt(0, 0, 0).unwrap()));
                    }
                    Query::Changes(token) => {
                        q.append_pair("syncToken", token);
                    }
                }
                if let Some(t) = &page_token {
                    q.append_pair("pageToken", t);
                }
            }
            let page: Value = Self::check(self.http.get(url).bearer_auth(self.token()?).send()?)?.json()?;
            for item in page["items"].as_array().into_iter().flatten() {
                match parse_item(cal_id, item) {
                    Some(Item::Event(ev)) => out.events.push(ev),
                    Some(Item::Cancelled(id)) => out.cancelled.push(id),
                    None => {}
                }
            }
            page_token = page["nextPageToken"].as_str().map(str::to_owned);
            if page_token.is_none() {
                // Only the last page carries the token for the next sync.
                out.next_token = page["nextSyncToken"].as_str().map(str::to_owned);
                break;
            }
        }
        Ok(out)
    }

    /// Creates or updates the event and returns its Google id.
    /// Creates or updates the event; returns its Google id and new etag.
    ///
    /// Updates send `If-Match` with the event's etag, so Google answers 412
    /// instead of overwriting someone else's newer change. New events carry
    /// an id derived from the local id, so retrying a create whose reply was
    /// lost can't produce a duplicate.
    fn save_event(&mut self, cal_id: &str, ev: &Event) -> Result<(String, Option<String>)> {
        let mut body = event_to_json(ev);
        let token = self.token()?;
        let req = match &ev.remote_id {
            Some(rid) => {
                let req = self.http.patch(self.url(&["calendars", cal_id, "events", rid]));
                match &ev.etag {
                    Some(etag) => req.header("If-Match", etag),
                    None => req,
                }
            }
            None => {
                if let Some(id) = client_event_id(&ev.id) {
                    body["id"] = json!(id);
                }
                self.http.post(self.url(&["calendars", cal_id, "events"]))
            }
        };
        let result = req.bearer_auth(token).json(&body).send().map_err(anyhow::Error::from).and_then(Self::check);
        let resp: Value = match result {
            Ok(resp) => resp.json()?,
            // Our id already exists: an earlier attempt got through.
            Err(e) if http_status(&e) == Some(409) && ev.remote_id.is_none() => {
                let id = client_event_id(&ev.id).ok_or(e)?;
                return match self.get_event(cal_id, &id)? {
                    Some(existing) => Ok((id, existing.etag)),
                    None => Err(anyhow!("Google reported event {id} as a duplicate but doesn't have it")),
                };
            }
            Err(e) => return Err(e),
        };
        let id = resp["id"].as_str().ok_or_else(|| anyhow!("Google returned no event id"))?.to_owned();
        Ok((id, resp["etag"].as_str().map(str::to_owned)))
    }

    /// Google's current copy of an event; `None` if it was deleted.
    fn get_event(&mut self, cal_id: &str, remote_id: &str) -> Result<Option<Event>> {
        let resp = self.http.get(self.url(&["calendars", cal_id, "events", remote_id])).bearer_auth(self.token()?).send()?;
        if matches!(resp.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        let v: Value = Self::check(resp)?.json()?;
        // Deleted events can linger with status "cancelled".
        Ok(event_from_json(cal_id, &v))
    }

    /// Deletes the event, only if it's still at `etag` when one is given.
    fn delete_event(&mut self, cal_id: &str, remote_id: &str, etag: Option<&str>) -> Result<()> {
        let mut req = self.http.delete(self.url(&["calendars", cal_id, "events", remote_id])).bearer_auth(self.token()?);
        if let Some(etag) = etag {
            req = req.header("If-Match", etag);
        }
        let resp = req.send()?;
        // Already gone is fine.
        if matches!(resp.status().as_u16(), 404 | 410) {
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

/// The Google id to request for a new event, derived from its local id
/// ("local:<uuid hex>"). Google accepts lowercase base32hex (a–v, 0–9),
/// 5–1024 characters, which hex UUIDs satisfy.
fn client_event_id(local_id: &str) -> Option<String> {
    let id = local_id.strip_prefix("local:")?;
    let ok = (5..=1024).contains(&id.len()) && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='v'));
    ok.then(|| id.to_owned())
}

enum Item {
    Event(Event),
    Cancelled(String),
}

fn parse_item(cal_id: &str, v: &Value) -> Option<Item> {
    if v["status"].as_str() == Some("cancelled") {
        return Some(Item::Cancelled(v["id"].as_str()?.to_owned()));
    }
    event_from_json(cal_id, v).map(Item::Event)
}

fn event_from_json(cal_id: &str, v: &Value) -> Option<Event> {
    if v["status"].as_str() == Some("cancelled") {
        return None;
    }
    let remote_id = v["id"].as_str()?.to_owned();
    let (start, all_day) = parse_when(&v["start"])?;
    let (end, _) = parse_when(&v["end"]).unwrap_or((start, all_day));
    let time_zone = if all_day { None } else { v["start"]["timeZone"].as_str().map(str::to_owned) };
    let recurrence = v["recurrence"].as_array().and_then(|lines| {
        let lines: Vec<String> = lines.iter().filter_map(|l| l.as_str().map(str::to_owned)).collect();
        recurrence::from_rrule(&lines, all_day, time_zone.as_deref().and_then(recurrence::parse_zone))
    });
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
        recurring_id: v["recurringEventId"].as_str().map(str::to_owned),
        pending: false,
        rev: 0,
        etag: v["etag"].as_str().map(str::to_owned),
        base: None,
        recurrence,
        time_zone,
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
    // Google needs a zone to repeat a timed event in; a floating series
    // repeats in this computer's zone.
    let zone = match (&ev.time_zone, &ev.recurrence) {
        _ if ev.all_day => None,
        (Some(z), _) => Some(z.clone()),
        (None, Some(_)) => recurrence::local_zone_name(),
        (None, None) => None,
    };
    let when = |t: NaiveDateTime| {
        if ev.all_day {
            json!({ "date": t.date().format("%Y-%m-%d").to_string() })
        } else {
            match &zone {
                Some(z) => json!({ "dateTime": to_rfc3339(t), "timeZone": z }),
                None => json!({ "dateTime": to_rfc3339(t) }),
            }
        }
    };
    let mut body = json!({
        "summary": ev.title,
        "location": ev.location,
        "description": ev.notes,
        "start": when(ev.start),
        "end": when(ev.end),
    });
    if let Some(rule) = &ev.recurrence {
        body["recurrence"] = json!(recurrence::to_rrule(rule, ev, zone.as_deref().unwrap_or("UTC")));
    }
    body
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
    fn time_zones_and_repeat_rules_round_trip() {
        let v = json!({"id": "s", "summary": "Standup",
            "start": {"dateTime": "2026-11-16T09:00:00-05:00", "timeZone": "America/New_York"},
            "end": {"dateTime": "2026-11-16T09:15:00-05:00", "timeZone": "America/New_York"},
            "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10"]});
        let ev = event_from_json("c", &v).unwrap();
        assert_eq!(ev.time_zone.as_deref(), Some("America/New_York"));
        let rule = ev.recurrence.as_ref().unwrap();
        assert_eq!((rule.freq, rule.end), (recurrence::Freq::Weekly, recurrence::RepeatEnd::Count(10)));

        let body = event_to_json(&ev);
        assert_eq!(body["start"]["timeZone"], "America/New_York");
        assert_eq!(body["end"]["timeZone"], "America/New_York");
        assert_eq!(body["recurrence"], json!(["RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10"]));
        // The same instant, whatever this computer's zone is.
        let sent = DateTime::parse_from_rfc3339(body["start"]["dateTime"].as_str().unwrap()).unwrap();
        assert_eq!(sent, DateTime::parse_from_rfc3339("2026-11-16T09:00:00-05:00").unwrap());

        // A plain event doesn't name a zone, and an all-day one never does.
        let plain = json!({"id": "p", "start": {"dateTime": "2026-11-14T18:00:00Z"}, "end": {"dateTime": "2026-11-14T19:00:00Z"}});
        let plain = event_from_json("c", &plain).unwrap();
        assert!(plain.time_zone.is_none() && plain.recurrence.is_none());
        assert!(event_to_json(&plain)["start"].get("timeZone").is_none());
        assert!(event_to_json(&plain).get("recurrence").is_none());
    }

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#9fe1e7"), Some([0x9f, 0xe1, 0xe7]));
        assert_eq!(parse_hex("nope"), None);
    }
}

#[cfg(test)]
mod http_tests;

#[cfg(test)]
mod emulator_tests;

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod failure_tests {
    use super::*;

    fn http(status: u16, message: &str) -> anyhow::Error {
        HttpError { status, message: message.into() }.into()
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(classify(&anyhow!("connection refused")), Failure::Retry);
        assert_eq!(classify(&http(503, "Backend Error")), Failure::Retry);
        assert_eq!(classify(&http(429, "Too Many Requests")), Failure::Retry);
        assert_eq!(classify(&http(403, "Rate Limit Exceeded")), Failure::Retry);
        assert_eq!(classify(&http(404, "Not Found")), Failure::Gone);
        assert_eq!(classify(&http(410, "Deleted")), Failure::Gone);
        assert_eq!(classify(&http(400, "Bad Request")), Failure::Rejected);
        assert_eq!(classify(&http(403, "Forbidden")), Failure::Rejected);
    }

    #[test]
    fn parses_cancelled_items() {
        let v = json!({"id": "abc", "status": "cancelled"});
        assert!(matches!(parse_item("c", &v), Some(Item::Cancelled(id)) if id == "abc"));
        let v = json!({"id": "i1", "recurringEventId": "series",
            "start": {"date": "2026-11-11"}, "end": {"date": "2026-11-12"}});
        match parse_item("c", &v) {
            Some(Item::Event(ev)) => assert_eq!(ev.recurring_id.as_deref(), Some("series")),
            _ => panic!("expected an event"),
        }
    }
}
