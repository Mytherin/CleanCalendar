//! What every calendar service has in common: how calendars and events on a
//! service are addressed, the commands the UI sends to the background worker
//! and the replies it gets, and the worker itself.
//!
//! Each service ([`crate::google`], [`crate::caldav`]) only implements
//! [`Remote`]: list calendars, list events, save, fetch, delete. Everything
//! else (retries, conflict detection, falling back to a full sync, merging,
//! offline changes) is shared: here for the network side, in
//! [`crate::sync::SyncEngine`] for the bookkeeping.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result};
use chrono::NaiveDate;

use crate::model::Event;
use crate::{caldav, google};

/// The account key of the Google account (there is at most one).
pub const GOOGLE: &str = "google";

/// A calendar on a service: which account, and the service's own id for it
/// (a Google calendar id, or a CalDAV collection URL).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CalRef {
    pub account: String,
    pub id: String,
}

impl CalRef {
    pub fn new(account: &str, id: &str) -> Self {
        CalRef { account: account.to_owned(), id: id.to_owned() }
    }

    pub fn google(id: &str) -> Self {
        Self::new(GOOGLE, id)
    }

    /// The calendar's id in the app: `<account>:<service id>`.
    pub fn local_id(&self) -> String {
        local_calendar_id(&self.account, &self.id)
    }

    /// The app's id for an event on this calendar.
    pub fn event_id(&self, remote_id: &str) -> String {
        format!("{}:{remote_id}", self.local_id())
    }
}

pub fn local_calendar_id(account: &str, calendar: &str) -> String {
    format!("{account}:{calendar}")
}

/// How the worker signs in to one account.
#[derive(Clone, Debug, PartialEq)]
pub enum Login {
    Google(google::Creds),
    CalDav(caldav::Login),
}

/// A calendar as a service lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteCalendar {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    pub read_only: bool,
    /// The account's main calendar (Google names the account after it).
    pub primary: bool,
}

/// What one calendar's sync returned.
#[derive(Debug)]
pub struct CalendarChanges {
    /// The service's id for the calendar.
    pub calendar_id: String,
    /// `true`: `events` is the complete set for the sync window.
    /// `false`: `events` and `cancelled` are only what changed.
    pub full: bool,
    pub events: Vec<Event>,
    /// Remote ids of deleted events, or of series whose occurrences all go.
    /// (An id may also be listed in `events`: everything under it is
    /// replaced by what `events` has.)
    pub cancelled: Vec<String>,
    pub next_token: Option<String>,
}

/// What to list.
#[derive(Clone, Copy, Debug)]
pub enum Query<'a> {
    /// Everything in [from, to).
    Window(NaiveDate, NaiveDate),
    /// What changed since this sync token.
    Changes(&'a str),
}

/// A calendar service. Remote ids, etags and sync tokens are opaque to the
/// rest of the app. Errors carry an [`HttpError`] when the service answered;
/// a sync token the service no longer accepts is reported as
/// [`TokenExpired`].
pub trait Remote {
    fn calendars(&mut self) -> Result<Vec<RemoteCalendar>>;

    /// Lists a calendar's events, by window or by changes. `window` is the
    /// range the app keeps; services that send repeating events as one
    /// series expand them into occurrences within it.
    fn events(&mut self, cal: &CalRef, query: Query, window: (NaiveDate, NaiveDate)) -> Result<CalendarChanges>;

    /// Creates the event (no `remote_id`) or updates it. An update based on
    /// an outdated version (see [`Event::etag`]) fails with HTTP 412. A
    /// create that is retried after its reply was lost must not duplicate
    /// the event. Returns the remote id and the new version tag.
    fn save(&mut self, cal: &CalRef, ev: &Event) -> Result<(String, Option<String>)>;

    /// The service's current copy, `None` if it was deleted.
    fn fetch(&mut self, cal: &CalRef, remote_id: &str) -> Result<Option<Event>>;

    /// Deletes the event; only if it is still at `etag` when one is given
    /// (HTTP 412 otherwise). Already deleted is not an error.
    fn delete(&mut self, cal: &CalRef, remote_id: &str, etag: Option<&str>) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Errors

/// A non-success HTTP response.
#[derive(Debug)]
pub struct HttpError {
    /// Who answered ("Google", "the CalDAV server").
    pub service: &'static str,
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} returned {}: {}", self.service, self.status, self.message)
    }
}

impl std::error::Error for HttpError {}

/// The service no longer accepts a sync token; a full sync is needed.
#[derive(Debug)]
pub struct TokenExpired;

impl std::fmt::Display for TokenExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the sync token expired")
    }
}

impl std::error::Error for TokenExpired {}

/// A change that can never be made as it is, however often it's retried
/// (e.g. the occurrence it edits is no longer part of its series, or the
/// server's copy can't be read).
#[derive(Debug)]
pub struct Unworkable(pub String);

impl std::fmt::Display for Unworkable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unworkable {}

pub fn http_status(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpError>().map(|h| h.status)
}

/// How an upload failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Offline, rate-limited, or a server error: try again later.
    Retry,
    /// The event (or its calendar) no longer exists on the service.
    Gone,
    /// The service refused the change; retrying won't help.
    Rejected,
    /// The service no longer accepts our sign-in (revoked, expired, or the
    /// client credentials changed): nothing works until the user connects
    /// again. Changes are kept for then.
    SignedOut,
}

/// The service refused our sign-in in a way retrying can't fix (for Google,
/// the token endpoint rejecting the refresh token).
#[derive(Debug)]
pub struct AuthError {
    pub service: &'static str,
    pub code: String,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} sign-in expired or was revoked ({}); connect again", self.service, self.code)
    }
}

impl std::error::Error for AuthError {}

pub fn classify(e: &anyhow::Error) -> Failure {
    if e.is::<Unworkable>() {
        return Failure::Rejected;
    }
    if e.is::<AuthError>() {
        return Failure::SignedOut;
    }
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

// ---------------------------------------------------------------------------
// Commands and replies

/// Commands sent from the UI to the worker.
#[derive(Debug)]
pub enum Cmd {
    /// Google's browser sign-in.
    ConnectGoogle { client_id: String, client_secret: String },
    /// Checks a CalDAV login by listing its calendars.
    ConnectCalDav { account: String, login: caldav::Login },
    /// Fetches the account's calendar list, then changes for each calendar:
    /// incremental when `tokens` (by service calendar id) has a sync token
    /// for it, otherwise everything in [`from`, `to`).
    Sync { account: String, login: Login, from: NaiveDate, to: NaiveDate, tokens: HashMap<String, String> },
    Save { login: Login, calendar: CalRef, event: Event },
    Delete {
        login: Login,
        calendar: CalRef,
        remote_id: String,
        /// Only delete this version; `None` deletes unconditionally.
        etag: Option<String>,
    },
}

/// Results sent from the worker back to the UI.
#[derive(Debug)]
pub enum Msg {
    /// Signed in. For Google, with the refresh token to keep.
    Connected { account: String, refresh_token: Option<String> },
    ConnectFailed { account: String, error: String },
    Synced { account: String, calendars: Vec<RemoteCalendar>, changes: Vec<CalendarChanges> },
    SyncFailed { account: String, error: String },
    Saved { local_id: String, calendar: CalRef, rev: u64, remote_id: String, etag: Option<String> },
    SaveFailed { local_id: String, calendar: CalRef, error: String, kind: Failure },
    /// The event changed on the service since we last saw it (or was
    /// deleted there: `server` is `None`). Carries the current copy.
    SaveConflict { local_id: String, server: Option<Event> },
    Deleted { remote_id: String },
    DeleteFailed { remote_id: String, calendar: CalRef, error: String, kind: Failure },
    /// Someone changed the event after we decided to delete it; nothing
    /// was deleted. Carries the current copy (`None` if it's gone).
    DeleteConflict { remote_id: String, calendar: CalRef, server: Option<Event> },
}

// ---------------------------------------------------------------------------
// The worker

/// The worker's open connections, one per account, plus what it needs to
/// make new ones.
pub struct Connections {
    pub google: google::Endpoints,
    pub open_browser: Box<dyn Fn(&str) -> Result<()> + Send>,
    clients: HashMap<String, (Login, Box<dyn Remote + Send>)>,
}

impl Connections {
    pub fn new(google: google::Endpoints, open_browser: Box<dyn Fn(&str) -> Result<()> + Send>) -> Self {
        Connections { google, open_browser, clients: HashMap::new() }
    }

    /// The client for an account, reused while its login stays the same
    /// (so access tokens and discovered URLs are kept).
    fn client(&mut self, account: &str, login: &Login) -> &mut (dyn Remote + Send) {
        let stale = self.clients.get(account).map_or(true, |(l, _)| l != login);
        if stale {
            let client: Box<dyn Remote + Send> = match login {
                Login::Google(creds) => Box::new(google::Client::new(creds.clone(), &self.google)),
                Login::CalDav(l) => Box::new(caldav::Client::new(l.clone())),
            };
            self.clients.insert(account.to_owned(), (login.clone(), client));
        }
        self.clients.get_mut(account).unwrap().1.as_mut()
    }
}

pub struct Worker {
    tx: Sender<Cmd>,
    pub rx: Receiver<Msg>,
    pub busy: usize,
}

impl Worker {
    pub fn spawn(ctx: eframe::egui::Context, google: google::Endpoints) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("sync".into())
            .spawn(move || {
                let open_browser = |url: &str| open::that(url).context("opening the browser");
                let mut conns = Connections::new(google, Box::new(open_browser));
                for cmd in cmd_rx {
                    // A panic on unexpected remote data must not end syncing
                    // for good (it would happen again on every launch):
                    // report it as this command's failure and carry on.
                    let fallback = panicked(&cmd);
                    let msg = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut conns, cmd)))
                        .unwrap_or(fallback);
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

/// The reply for a command whose handling panicked. Saves and deletes are
/// reported as refused, not as transient, so they aren't retried forever.
fn panicked(cmd: &Cmd) -> Msg {
    let error = "Unexpected data from the server (see the log)".to_owned();
    match cmd {
        Cmd::ConnectGoogle { .. } => Msg::ConnectFailed { account: GOOGLE.into(), error },
        Cmd::ConnectCalDav { account, .. } => Msg::ConnectFailed { account: account.clone(), error },
        Cmd::Sync { account, .. } => Msg::SyncFailed { account: account.clone(), error },
        Cmd::Save { calendar, event, .. } => {
            Msg::SaveFailed { local_id: event.id.clone(), calendar: calendar.clone(), error, kind: Failure::Rejected }
        }
        Cmd::Delete { calendar, remote_id, .. } => Msg::DeleteFailed {
            remote_id: remote_id.clone(),
            calendar: calendar.clone(),
            error,
            kind: Failure::Rejected,
        },
    }
}

/// Executes one command; what the worker thread does for each message.
pub fn run(conns: &mut Connections, cmd: Cmd) -> Msg {
    let text = |e: anyhow::Error| format!("{e:#}");
    match cmd {
        Cmd::ConnectGoogle { client_id, client_secret } => {
            match google::authorize(&conns.google, &client_id, &client_secret, conns.open_browser.as_ref()) {
                Ok(token) => Msg::Connected { account: GOOGLE.into(), refresh_token: Some(token) },
                Err(e) => Msg::ConnectFailed { account: GOOGLE.into(), error: text(e) },
            }
        }
        Cmd::ConnectCalDav { account, login } => {
            match conns.client(&account, &Login::CalDav(login)).calendars() {
                Ok(_) => Msg::Connected { account, refresh_token: None },
                Err(e) => Msg::ConnectFailed { account, error: caldav::explain(&e).unwrap_or_else(|| text(e)) },
            }
        }
        Cmd::Sync { account, login, from, to, tokens } => {
            match sync_all(conns.client(&account, &login), &account, from, to, &tokens) {
                Ok((calendars, changes)) => Msg::Synced { account, calendars, changes },
                Err(e) => Msg::SyncFailed { account, error: text(e) },
            }
        }
        Cmd::Save { login, calendar, event } => {
            let c = conns.client(&calendar.account, &login);
            let failed = |e: anyhow::Error, local_id: String, calendar: CalRef| Msg::SaveFailed {
                local_id,
                calendar,
                kind: classify(&e),
                error: text(e),
            };
            match c.save(&calendar, &event) {
                Ok((remote_id, etag)) => Msg::Saved { local_id: event.id, calendar, rev: event.rev, remote_id, etag },
                // Changed on the service since we last saw it: fetch their copy.
                Err(e) if http_status(&e) == Some(412) && event.remote_id.is_some() => {
                    let rid = event.remote_id.clone().unwrap_or_default();
                    match c.fetch(&calendar, &rid) {
                        Ok(server) => Msg::SaveConflict { local_id: event.id, server },
                        Err(e) => failed(e, event.id, calendar),
                    }
                }
                // Updating an event that was deleted there.
                Err(e) if classify(&e) == Failure::Gone && event.remote_id.is_some() => {
                    Msg::SaveConflict { local_id: event.id, server: None }
                }
                Err(e) => failed(e, event.id, calendar),
            }
        }
        Cmd::Delete { login, calendar, remote_id, etag } => {
            let c = conns.client(&calendar.account, &login);
            match c.delete(&calendar, &remote_id, etag.as_deref()) {
                Ok(()) => Msg::Deleted { remote_id },
                // Changed there after we decided to delete it.
                Err(e) if http_status(&e) == Some(412) => match c.fetch(&calendar, &remote_id) {
                    Ok(server) => Msg::DeleteConflict { remote_id, calendar, server },
                    Err(e) => Msg::DeleteFailed { remote_id, calendar, kind: classify(&e), error: text(e) },
                },
                Err(e) => Msg::DeleteFailed { remote_id, calendar, kind: classify(&e), error: text(e) },
            }
        }
    }
}

/// Lists the account's calendars and syncs each one: incrementally where
/// there's a token, falling back to a full sync when the token expired.
pub fn sync_all(
    c: &mut dyn Remote,
    account: &str,
    from: NaiveDate,
    to: NaiveDate,
    tokens: &HashMap<String, String>,
) -> Result<(Vec<RemoteCalendar>, Vec<CalendarChanges>)> {
    let calendars = c.calendars()?;
    let mut out = Vec::new();
    for cal in &calendars {
        let r = CalRef::new(account, &cal.id);
        let incremental = match tokens.get(&cal.id) {
            Some(token) => match c.events(&r, Query::Changes(token), (from, to)) {
                Ok(changes) => Some(changes),
                Err(e) if e.is::<TokenExpired>() => None,
                Err(e) if refused(&e) => {
                    eprintln!("Skipping calendar {}: {e:#}", cal.name);
                    continue;
                }
                Err(e) => return Err(e),
            },
            None => None,
        };
        let changes = match incremental {
            Some(changes) => changes,
            None => match c.events(&r, Query::Window(from, to), (from, to)) {
                Ok(changes) => changes,
                // One calendar the server won't list (e.g. a broken shared
                // one) shouldn't stop the others; it keeps what it had.
                Err(e) if refused(&e) => {
                    eprintln!("Skipping calendar {}: {e:#}", cal.name);
                    continue;
                }
                Err(e) => return Err(e),
            },
        };
        out.push(changes);
    }
    Ok((calendars, out))
}

/// The server refused this request for good (not offline, not signed out).
fn refused(e: &anyhow::Error) -> bool {
    e.is::<Unworkable>() || http_status(e).is_some_and(|s| (400..500).contains(&s) && !matches!(s, 401 | 408 | 429))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, message: &str) -> anyhow::Error {
        HttpError { service: "Test", status, message: message.into() }.into()
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(classify(&anyhow::anyhow!("connection refused")), Failure::Retry);
        assert_eq!(classify(&http(503, "Backend Error")), Failure::Retry);
        assert_eq!(classify(&http(429, "Too Many Requests")), Failure::Retry);
        assert_eq!(classify(&http(403, "Rate Limit Exceeded")), Failure::Retry);
        assert_eq!(classify(&http(401, "Unauthorized")), Failure::Retry);
        assert_eq!(classify(&http(404, "Not Found")), Failure::Gone);
        assert_eq!(classify(&http(410, "Deleted")), Failure::Gone);
        assert_eq!(classify(&http(400, "Bad Request")), Failure::Rejected);
        assert_eq!(classify(&http(403, "Forbidden")), Failure::Rejected);
        assert_eq!(classify(&Unworkable("series changed".into()).into()), Failure::Rejected);
        assert_eq!(http_status(&http(412, "x")), Some(412));
        assert_eq!(http_status(&anyhow::anyhow!("x")), None);
    }

    #[test]
    fn ids_keep_the_format_google_data_was_saved_with() {
        let g = CalRef::google("me@example.com");
        assert_eq!(g.local_id(), "google:me@example.com");
        assert_eq!(g.event_id("abc"), "google:me@example.com:abc");
        let d = CalRef::new("caldav-1a2b3c4d", "https://dav.example.com/cal/home/");
        assert_eq!(d.local_id(), "caldav-1a2b3c4d:https://dav.example.com/cal/home/");
    }

    /// A scripted service, to test the worker's shared logic on its own.
    #[derive(Default)]
    struct Script {
        token_expired: bool,
        save_status: Option<u16>,
        delete_status: Option<u16>,
        server_copy: Option<Event>,
        calls: Vec<String>,
    }

    fn fail(status: u16) -> anyhow::Error {
        HttpError { service: "Test", status, message: "no".into() }.into()
    }

    impl Remote for Script {
        fn calendars(&mut self) -> Result<Vec<RemoteCalendar>> {
            self.calls.push("calendars".into());
            Ok(vec![
                RemoteCalendar { id: "a".into(), name: "A".into(), color: [0; 3], read_only: false, primary: true },
                RemoteCalendar { id: "b".into(), name: "B".into(), color: [0; 3], read_only: true, primary: false },
            ])
        }
        fn events(&mut self, cal: &CalRef, query: Query, _: (NaiveDate, NaiveDate)) -> Result<CalendarChanges> {
            let full = match query {
                Query::Changes(t) => {
                    self.calls.push(format!("changes {} {t}", cal.id));
                    if self.token_expired {
                        return Err(TokenExpired.into());
                    }
                    false
                }
                Query::Window(..) => {
                    self.calls.push(format!("window {}", cal.id));
                    true
                }
            };
            Ok(CalendarChanges { calendar_id: cal.id.clone(), full, events: vec![], cancelled: vec![], next_token: Some("t2".into()) })
        }
        fn save(&mut self, _: &CalRef, _: &Event) -> Result<(String, Option<String>)> {
            match self.save_status {
                Some(s) => Err(fail(s)),
                None => Ok(("r1".into(), Some("e1".into()))),
            }
        }
        fn fetch(&mut self, _: &CalRef, rid: &str) -> Result<Option<Event>> {
            self.calls.push(format!("fetch {rid}"));
            Ok(self.server_copy.clone())
        }
        fn delete(&mut self, _: &CalRef, _: &str, _: Option<&str>) -> Result<()> {
            match self.delete_status {
                Some(s) => Err(fail(s)),
                None => Ok(()),
            }
        }
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    #[test]
    fn sync_uses_tokens_where_it_has_them() {
        let mut s = Script::default();
        let tokens = HashMap::from([("a".to_string(), "t1".to_string())]);
        let (cals, changes) = sync_all(&mut s, "acct", day(1), day(30), &tokens).unwrap();
        assert_eq!(cals.len(), 2);
        assert_eq!(s.calls, ["calendars", "changes a t1", "window b"]);
        assert!(!changes[0].full && changes[1].full);
    }

    #[test]
    fn expired_token_falls_back_to_a_full_sync() {
        let mut s = Script { token_expired: true, ..Default::default() };
        let tokens = HashMap::from([("a".to_string(), "old".to_string())]);
        let (_, changes) = sync_all(&mut s, "acct", day(1), day(30), &tokens).unwrap();
        assert_eq!(s.calls, ["calendars", "changes a old", "window a", "window b"]);
        assert!(changes[0].full);
    }

    fn connections_with(script: Script) -> Connections {
        let mut c = Connections::new(google::Endpoints::google(), Box::new(|_: &str| panic!("no browser")));
        let login = Login::CalDav(caldav::Login::default());
        c.clients.insert("acct".into(), (login, Box::new(script)));
        c
    }

    fn event(remote_id: Option<&str>) -> Event {
        let mut ev = crate::model::tests::event("local:1", "x:a", "Lunch");
        ev.remote_id = remote_id.map(str::to_owned);
        ev.rev = 4;
        ev
    }

    fn save(remote_id: Option<&str>) -> Cmd {
        Cmd::Save { login: Login::CalDav(caldav::Login::default()), calendar: CalRef::new("acct", "a"), event: event(remote_id) }
    }

    #[test]
    fn worker_reports_saves_with_the_revision_they_carried() {
        let mut c = connections_with(Script::default());
        match run(&mut c, save(None)) {
            Msg::Saved { local_id, rev, remote_id, etag, calendar } => {
                assert_eq!((local_id.as_str(), rev, remote_id.as_str()), ("local:1", 4, "r1"));
                assert_eq!((etag.as_deref(), calendar), (Some("e1"), CalRef::new("acct", "a")));
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn stale_update_comes_back_as_a_conflict_with_the_server_copy() {
        let theirs = event(Some("r1"));
        let mut c = connections_with(Script { save_status: Some(412), server_copy: Some(theirs), ..Default::default() });
        assert!(matches!(run(&mut c, save(Some("r1"))), Msg::SaveConflict { server: Some(_), .. }));

        // Updating something deleted there: a conflict without a copy.
        let mut c = connections_with(Script { save_status: Some(404), ..Default::default() });
        assert!(matches!(run(&mut c, save(Some("r1"))), Msg::SaveConflict { server: None, .. }));

        // A failed create is just a failure.
        let mut c = connections_with(Script { save_status: Some(404), ..Default::default() });
        assert!(matches!(run(&mut c, save(None)), Msg::SaveFailed { kind: Failure::Gone, .. }));
        let mut c = connections_with(Script { save_status: Some(400), ..Default::default() });
        assert!(matches!(run(&mut c, save(Some("r1"))), Msg::SaveFailed { kind: Failure::Rejected, .. }));
        let mut c = connections_with(Script { save_status: Some(503), ..Default::default() });
        assert!(matches!(run(&mut c, save(Some("r1"))), Msg::SaveFailed { kind: Failure::Retry, .. }));
    }

    #[test]
    fn stale_delete_comes_back_as_a_conflict() {
        let delete = || Cmd::Delete {
            login: Login::CalDav(caldav::Login::default()),
            calendar: CalRef::new("acct", "a"),
            remote_id: "r1".into(),
            etag: Some("e0".into()),
        };
        let mut c = connections_with(Script { delete_status: Some(412), server_copy: Some(event(Some("r1"))), ..Default::default() });
        assert!(matches!(run(&mut c, delete()), Msg::DeleteConflict { server: Some(_), .. }));
        let mut c = connections_with(Script { delete_status: Some(500), ..Default::default() });
        assert!(matches!(run(&mut c, delete()), Msg::DeleteFailed { kind: Failure::Retry, .. }));
        let mut c = connections_with(Script::default());
        assert!(matches!(run(&mut c, delete()), Msg::Deleted { .. }));
    }
}
