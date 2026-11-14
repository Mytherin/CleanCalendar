//! CalDAV (RFC 4791) accounts: iCloud, and any other CalDAV server
//! (Fastmail, Nextcloud, Radicale, …).
//!
//! The client implements [`Remote`], so syncing, retries and conflict
//! handling are the same as for Google (see [`crate::remote`] and
//! [`crate::sync`]). What's CalDAV-specific:
//!
//! - **Discovery**: from the server URL, `current-user-principal`, then the
//!   principal's `calendar-home-set`, then the calendars in it. iCloud
//!   answers on caldav.icloud.com and points to the account's own host
//!   (`pNN-caldav.icloud.com`); hrefs are resolved against whichever URL
//!   answered, and redirects are followed by hand so they keep the method.
//! - **Full sync**: the collection's `sync-token`, then a `calendar-query`
//!   for the window. **Incremental**: a `sync-collection` report (RFC 6578)
//!   for what changed, then a `calendar-multiget` for the changed data.
//! - **Writes** read the resource first, change only what the app owns (see
//!   [`ical`]) and write it back with `If-Match`, so nothing else in it is
//!   lost and someone else's newer change is never overwritten.
//!
//! Sign-in is HTTP Basic over HTTPS; for iCloud with an app-specific
//! password.

pub mod ical;
pub mod xml;

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod server_tests;

use std::time::Duration as StdDuration;

use anyhow::{anyhow, Result};
use chrono::{Duration, NaiveDate, NaiveTime};
use reqwest::blocking::Response;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use url::Url;

use crate::model::Event;
use crate::remote::{http_status, CalRef, CalendarChanges, HttpError, Query, Remote, RemoteCalendar, TokenExpired, Unworkable};
use xml::{CALDAV, DAV, ICAL};

/// Where iCloud's CalDAV service starts.
pub const ICLOUD: &str = "https://caldav.icloud.com/";

const SERVICE: &str = "The CalDAV server";
const USER_AGENT: &str = concat!("CleanCalendar/", env!("CARGO_PKG_VERSION"));
/// Changed resources fetched per `calendar-multiget`.
const MULTIGET_BATCH: usize = 50;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Login {
    /// Where discovery starts, e.g. "https://caldav.icloud.com/".
    pub server: String,
    pub username: String,
    pub password: String,
}

/// A user-facing explanation for a failed sign-in, when there's a better
/// one than the raw error.
pub fn explain(e: &anyhow::Error) -> Option<String> {
    match http_status(e)? {
        401 | 403 => Some(
            "The server didn't accept that user name and password. For iCloud, sign in with your Apple ID \
             and an app-specific password (account.apple.com → Sign-In and Security → App-Specific Passwords)."
                .into(),
        ),
        404 => Some("No calendars were found at that address.".into()),
        _ => None,
    }
}

fn dav_error(status: u16, message: String) -> anyhow::Error {
    HttpError { service: SERVICE, status, message }.into()
}

/// The server URL as typed: "caldav.example.com" → "https://caldav.example.com/".
pub fn normalize_server(s: &str) -> Result<Url> {
    let s = s.trim();
    let with_scheme = if s.contains("://") { s.to_owned() } else { format!("https://{s}") };
    let mut url = Url::parse(&with_scheme).map_err(|e| anyhow!("“{s}” isn't a valid server address ({e})"))?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

pub struct Client {
    http: reqwest::blocking::Client,
    login: Login,
    /// The calendar home, once discovered.
    home: Option<Url>,
}

impl Client {
    pub fn new(login: Login) -> Self {
        Client {
            http: reqwest::blocking::Client::builder()
                .timeout(StdDuration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(USER_AGENT)
                .build()
                .expect("http client"),
            login,
            home: None,
        }
    }

    /// Sends a request, following redirects with the same method and body.
    /// Returns the URL that answered and the response.
    fn send(&self, method: &str, url: &Url, headers: &[(&str, &str)], body: Option<(&str, String)>) -> Result<(Url, Response)> {
        let mut method = Method::from_bytes(method.as_bytes())?;
        let mut body = body;
        let mut url = url.clone();
        let origin = url.clone();
        for _ in 0..8 {
            let mut h = HeaderMap::new();
            for (k, v) in headers {
                h.insert(HeaderName::from_bytes(k.as_bytes())?, HeaderValue::from_str(v)?);
            }
            let mut req = self.http.request(method.clone(), url.clone()).headers(h);
            let has_login = !self.login.username.is_empty() || !self.login.password.is_empty();
            if has_login && may_receive_login(&origin, &url) {
                req = req.basic_auth(&self.login.username, Some(&self.login.password));
            }
            if let Some((ctype, text)) = &body {
                req = req.header("Content-Type", *ctype).body(text.clone());
            }
            let resp = req.send()?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if let Some(loc) = resp.headers().get("Location").and_then(|l| l.to_str().ok()) {
                    let next = url.join(loc)?;
                    // Never send the password on in the clear.
                    if url.scheme() == "https" && next.scheme() != "https" {
                        return Err(anyhow!("{url} redirected to an insecure address ({next})"));
                    }
                    // 303 means "go GET this instead".
                    if status == 303 {
                        method = Method::GET;
                        body = None;
                    }
                    url = next;
                    continue;
                }
            }
            return Ok((url, resp));
        }
        Err(anyhow!("too many redirects from {url}"))
    }

    fn check(resp: Response) -> Result<Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let reason = resp.status().canonical_reason().unwrap_or("").to_owned();
        let body = resp.text().unwrap_or_default();
        // Servers often explain with a DAV:error body naming a precondition.
        let detail = xml::parse(&body)
            .ok()
            .filter(|n| n.is(DAV, "error"))
            .and_then(|n| n.children.first().map(|c| c.name.clone()))
            .unwrap_or_default();
        let message = [reason, detail].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": ");
        Err(dav_error(status, message))
    }

    fn propfind(&self, url: &Url, depth: &str, body: &str) -> Result<(Url, Vec<xml::Response>)> {
        let (at, resp) =
            self.send("PROPFIND", url, &[("Depth", depth)], Some(("application/xml; charset=utf-8", body.to_owned())))?;
        let text = Self::check(resp)?.text()?;
        Ok((at, xml::multistatus(&text)?.0))
    }

    fn report(&self, url: &Url, depth: &str, body: String) -> Result<(Vec<xml::Response>, Option<String>)> {
        let (_, resp) = self.send("REPORT", url, &[("Depth", depth)], Some(("application/xml; charset=utf-8", body)))?;
        let text = Self::check(resp)?.text()?;
        xml::multistatus(&text)
    }

    /// The first href-valued property named in a PROPFIND answer.
    fn href_prop(rs: &[xml::Response], ns: &str, name: &str) -> Option<String> {
        rs.iter().find_map(|r| r.prop(ns, name).and_then(xml::Node::href))
    }

    /// Finds (once) the calendar home: principal, then its home set.
    fn home(&mut self) -> Result<Url> {
        if let Some(h) = &self.home {
            return Ok(h.clone());
        }
        let base = normalize_server(&self.login.server)?;
        // The principal, from the server URL or its well-known address.
        let mut principal = None;
        for start in [base.clone(), base.join("/.well-known/caldav")?] {
            match self.propfind(&start, "0", xml::PRINCIPAL) {
                Ok((at, rs)) => {
                    if let Some(href) = Self::href_prop(&rs, DAV, "current-user-principal") {
                        principal = Some(at.join(&href)?);
                        break;
                    }
                }
                Err(e) if matches!(http_status(&e), Some(401)) => return Err(e),
                Err(_) => {}
            }
        }
        // Without a principal, the URL may be the home (or a principal) itself.
        let principal = principal.unwrap_or_else(|| base.clone());
        let home = match self.propfind(&principal, "0", xml::HOME_SET) {
            Ok((at, rs)) => match Self::href_prop(&rs, CALDAV, "calendar-home-set") {
                Some(href) => at.join(&href)?,
                None => principal,
            },
            Err(e) if matches!(http_status(&e), Some(401 | 403)) => return Err(e),
            Err(_) => principal,
        };
        self.home = Some(home.clone());
        Ok(home)
    }

    fn calendar_url(cal: &CalRef) -> Result<Url> {
        let mut url = Url::parse(&cal.id).map_err(|e| anyhow!("bad calendar URL {}: {e}", cal.id))?;
        // Also for calendars stored before collection URLs were normalized.
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(url)
    }

    /// An href as the app keeps it: the resolved path.
    fn norm(base: &Url, href: &str) -> Result<String> {
        Ok(base.join(href)?.path().to_owned())
    }

    fn resource_url(cal_url: &Url, href: &str) -> Result<Url> {
        Ok(cal_url.join(href)?)
    }

    /// GETs a resource: `None` if it's gone, else its data and etag.
    fn get(&self, url: &Url) -> Result<Option<(ical::Component, Option<String>)>> {
        let (_, resp) = self.send("GET", url, &[("Accept", "text/calendar")], None)?;
        if matches!(resp.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        let resp = Self::check(resp)?;
        let etag = resp.headers().get("ETag").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let ics = ical::parse_bytes(&resp.bytes()?).map_err(|e| Unworkable(format!("the server's copy of {url} can't be read: {e:#}")))?;
        Ok(Some((ics, etag)))
    }

    /// PUTs a resource with a precondition and returns the new etag.
    fn put(&self, url: &Url, cal: &ical::Component, precondition: (&str, &str)) -> Result<Option<String>> {
        let (_, resp) = self.send("PUT", url, &[precondition], Some(("text/calendar; charset=utf-8", ical::write(cal))))?;
        let resp = Self::check(resp)?;
        let etag = resp.headers().get("ETag").and_then(|v| v.to_str().ok()).map(str::to_owned);
        // Some servers (iCloud among them) change what they store and don't
        // say which version that is; ask.
        match etag {
            Some(e) => Ok(Some(e)),
            None => Ok(self.get(url)?.and_then(|(_, e)| e)),
        }
    }

    /// The events of a multistatus with calendar data.
    fn parse_events(&self, cal: &CalRef, cal_url: &Url, rs: &[xml::Response], window: (NaiveDate, NaiveDate)) -> Vec<Event> {
        let mut out = Vec::new();
        for r in rs {
            let Some(data) = r.prop(CALDAV, "calendar-data") else { continue };
            let Ok(href) = Self::norm(cal_url, &r.href) else { continue };
            let etag = r.prop_text(DAV, "getetag");
            match ical::parse(&data.text) {
                Ok(ics) => out.extend(ical::events(cal, &href, etag.as_deref(), &ics, window.0, window.1)),
                Err(e) => eprintln!("Skipping unreadable event {href}: {e:#}"),
            }
        }
        out
    }
}

/// "#RRGGBB" or "#RRGGBBAA" → RGB.
fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().trim_start_matches('#');
    // ASCII only: slicing a multi-byte character would panic.
    if !hex.is_ascii() || (hex.len() != 6 && hex.len() != 8) {
        return None;
    }
    let n = u32::from_str_radix(&hex[..6], 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

/// "20261114T000000Z" for midnight UTC of `d`.
fn utc_stamp(d: NaiveDate) -> String {
    format!("{}Z", d.and_time(NaiveTime::MIN).format("%Y%m%dT%H%M%S"))
}

/// Whether a request redirected from `origin` to `to` may carry the login:
/// the same host, or a sibling under the same parent domain (iCloud sends
/// caldav.icloud.com to p42-caldav.icloud.com). Elsewhere it would hand the
/// password to whatever host a redirect names.
fn may_receive_login(origin: &Url, to: &Url) -> bool {
    let (Some(a), Some(b)) = (origin.host_str(), to.host_str()) else { return false };
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    // "caldav.icloud.com" → "icloud.com". IP addresses and single-label
    // hosts only match themselves. Without a public-suffix list, a parent
    // that looks like a country's second level ("co.uk", "com.au") is not
    // trusted either, so "dav.example.co.uk" can't send it to all of "co.uk".
    if a.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    let Some((_, parent)) = a.split_once('.') else { return false };
    let labels: Vec<&str> = parent.split('.').collect();
    let looks_like_public_suffix = labels.len() < 2
        || (labels.len() == 2 && labels[1].len() == 2 && labels[0].len() <= 3);
    !looks_like_public_suffix && b.to_ascii_lowercase().ends_with(&format!(".{}", parent.to_ascii_lowercase()))
}

/// The UID for a new event: its local id's hex part, so a retried create
/// lands on the same resource.
/// Other ids (an event moved here from another calendar, or recreated after
/// a delete elsewhere, keeps its old id) get a UID derived from that id, so
/// a retry also lands on the same resource instead of duplicating it.
fn uid_for(local_id: &str) -> String {
    match local_id.strip_prefix("local:") {
        Some(hex) if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_alphanumeric()) => hex.to_owned(),
        _ => {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(local_id.as_bytes());
            digest[..16].iter().map(|b| format!("{b:02x}")).collect()
        }
    }
}

impl Remote for Client {
    fn calendars(&mut self) -> Result<Vec<RemoteCalendar>> {
        let home = self.home()?;
        let (at, rs) = self.propfind(&home, "1", xml::CALENDARS)?;
        let mut out: Vec<(i64, RemoteCalendar)> = Vec::new();
        for r in &rs {
            let Ok(mut url) = at.join(&r.href) else { continue };
            // A collection is a folder: without the slash, joining an event's
            // name to it would replace its last segment (PUT /cal/x.ics
            // instead of /cal/work/x.ics).
            if !url.path().ends_with('/') {
                let path = format!("{}/", url.path());
                url.set_path(&path);
            }
            if url.path() == home.path() {
                continue;
            }
            let is_calendar = r.prop(DAV, "resourcetype").is_some_and(|t| t.child(CALDAV, "calendar").is_some());
            if !is_calendar {
                continue;
            }
            // Reminders lists (VTODO only) live next to calendars on iCloud.
            if let Some(set) = r.prop(CALDAV, "supported-calendar-component-set") {
                let mut comps = set.children_named(CALDAV, "comp").filter_map(|c| c.attr("name")).peekable();
                if comps.peek().is_some() && !comps.any(|c| c.eq_ignore_ascii_case("VEVENT")) {
                    continue;
                }
            }
            let read_only = match r.prop(DAV, "current-user-privilege-set") {
                Some(p) => !["all", "write", "write-content", "bind"].iter().any(|w| p.find(DAV, w).is_some()),
                None => false,
            };
            let name = r.prop_text(DAV, "displayname").unwrap_or_else(|| {
                url.path_segments().and_then(|s| s.filter(|x| !x.is_empty()).last()).unwrap_or("Calendar").to_owned()
            });
            let order = r.prop_text(ICAL, "calendar-order").and_then(|o| o.parse().ok()).unwrap_or(i64::MAX);
            out.push((
                order,
                RemoteCalendar {
                    id: url.to_string(),
                    name,
                    color: r.prop_text(ICAL, "calendar-color").as_deref().and_then(parse_color).unwrap_or([0x1B, 0xAD, 0xF8]),
                    read_only,
                    primary: false,
                },
            ));
        }
        out.sort_by(|a, b| (a.0, a.1.read_only, a.1.name.to_lowercase()).cmp(&(b.0, b.1.read_only, b.1.name.to_lowercase())));
        let mut cals: Vec<RemoteCalendar> = out.into_iter().map(|(_, c)| c).collect();
        if let Some(first) = cals.iter_mut().find(|c| !c.read_only) {
            first.primary = true;
        }
        Ok(cals)
    }

    fn events(&mut self, cal: &CalRef, query: Query, window: (NaiveDate, NaiveDate)) -> Result<CalendarChanges> {
        let cal_url = Self::calendar_url(cal)?;
        match query {
            Query::Window(from, to) => {
                // The token first, so nothing that changes during the query
                // is missed next time.
                let token = match self.propfind(&cal_url, "0", xml::SYNC_TOKEN) {
                    Ok((_, rs)) => rs.iter().find_map(|r| r.prop_text(DAV, "sync-token")),
                    Err(e) if matches!(http_status(&e), Some(401)) => return Err(e),
                    Err(_) => None,
                };
                let (rs, _) = self.report(
                    &cal_url,
                    "1",
                    xml::calendar_query(&utc_stamp(from - Duration::days(1)), &utc_stamp(to + Duration::days(1))),
                )?;
                Ok(CalendarChanges {
                    calendar_id: cal.id.clone(),
                    full: true,
                    events: self.parse_events(cal, &cal_url, &rs, window),
                    cancelled: Vec::new(),
                    next_token: token,
                    skipped: None,
                })
            }
            Query::Changes(token) => {
                let (rs, next) = match self.report(&cal_url, "0", xml::sync_collection(token)) {
                    Ok(x) => x,
                    // Invalid or expired token (403/409/412 valid-sync-token),
                    // or a server that can't do this report: sync fully.
                    Err(e) if matches!(http_status(&e), Some(400 | 403 | 404 | 405 | 409 | 412 | 501)) => {
                        return Err(TokenExpired.into())
                    }
                    Err(e) => return Err(e),
                };
                let mut changed = Vec::new();
                let mut cancelled = Vec::new();
                for r in &rs {
                    let Ok(href) = Self::norm(&cal_url, &r.href) else { continue };
                    if href == cal_url.path() {
                        continue;
                    }
                    if matches!(r.status, Some(404 | 410)) {
                        cancelled.push(href);
                    } else if r.prop(DAV, "getetag").is_some() || r.status.is_none_or(|s| s < 300) {
                        changed.push(href);
                    }
                }
                let mut events = Vec::new();
                for chunk in changed.chunks(MULTIGET_BATCH) {
                    let (rs, _) = self.report(&cal_url, "1", xml::multiget(chunk))?;
                    events.extend(self.parse_events(cal, &cal_url, &rs, window));
                }
                // Everything under a changed resource is replaced by what it
                // has now (an occurrence may have been removed).
                cancelled.extend(changed);
                Ok(CalendarChanges { calendar_id: cal.id.clone(), full: false, events, cancelled, next_token: next, skipped: None })
            }
        }
    }

    fn save(&mut self, cal: &CalRef, ev: &Event) -> Result<(String, Option<String>)> {
        let cal_url = Self::calendar_url(cal)?;
        let Some(rid) = &ev.remote_id else {
            // New: a resource named after its UID; If-None-Match makes a
            // retried create (whose reply was lost) find the first one.
            let uid = uid_for(&ev.id);
            let url = cal_url.join(&format!("{uid}.ics"))?;
            let ics = ical::put(None, ev, &uid, None)?;
            let href = url.path().to_owned();
            return match self.put(&url, &ics, ("If-None-Match", "*")) {
                Ok(etag) => Ok((href, etag)),
                Err(e) if http_status(&e) == Some(412) => match self.get(&url)? {
                    Some((_, etag)) => Ok((href, etag)),
                    None => Err(e),
                },
                Err(e) => Err(e),
            };
        };
        let (href, key) = ical::split_remote_id(rid);
        let url = Self::resource_url(&cal_url, href)?;
        let Some((existing, server_etag)) = self.get(&url)? else {
            return Err(dav_error(404, "Not Found".into()));
        };
        // The event isn't in the resource any more as we know it: the series
        // was changed elsewhere so this occurrence no longer exists, or a
        // plain event became a repeating one. Recreating it would duplicate
        // it, so the server's version wins.
        match ical::find(cal, rid, server_etag.as_deref(), &existing) {
            // Stale unless the server still has the version we started from,
            // or what changed in the resource isn't this event (another
            // occurrence).
            Some(theirs) => {
                if ev.etag.is_some() && ev.etag != server_etag && ev.base.as_ref() != Some(&theirs.fields()) {
                    return Err(dav_error(412, "Precondition Failed: changed on the server".into()));
                }
            }
            // A series made here, edited again before a sync brought back its
            // occurrences: the whole resource is ours to rewrite, as long as
            // nobody changed it since.
            None if key.is_none() && ev.recurrence.is_some() && ical::is_series(&existing) => {
                if ev.etag.is_some() && ev.etag != server_etag {
                    return Err(dav_error(412, "Precondition Failed: changed on the server".into()));
                }
            }
            None => {
                return Err(Unworkable("it was changed into a different repeating event on another device".into()).into())
            }
        }
        let uid = ical::uid(&existing).unwrap_or_else(|| uid_for(&ev.id));
        let ics = ical::put(Some(existing), ev, &uid, key).map_err(|e| Unworkable(format!("{e:#}")))?;
        let etag = match &server_etag {
            Some(e) => self.put(&url, &ics, ("If-Match", e))?,
            None => self.put(&url, &ics, ("If-Match", "*"))?,
        };
        Ok((rid.clone(), etag))
    }

    fn fetch(&mut self, cal: &CalRef, remote_id: &str) -> Result<Option<Event>> {
        let cal_url = Self::calendar_url(cal)?;
        let (href, key) = ical::split_remote_id(remote_id);
        Ok(match self.get(&Self::resource_url(&cal_url, href)?)? {
            // A whole series isn't one event the merge could compare; saying
            // it's gone would make the caller recreate it as a duplicate.
            Some((ics, _)) if key.is_none() && ical::is_series(&ics) => {
                return Err(Unworkable("the repeating event was changed on another device".into()).into())
            }
            Some((ics, etag)) => ical::find(cal, remote_id, etag.as_deref(), &ics),
            None => None,
        })
    }

    fn delete(&mut self, cal: &CalRef, remote_id: &str, etag: Option<&str>) -> Result<Option<String>> {
        let cal_url = Self::calendar_url(cal)?;
        let (href, key) = ical::split_remote_id(remote_id);
        let url = Self::resource_url(&cal_url, href)?;
        if let Some(key) = key {
            // One occurrence: take it out of the series.
            let Some((mut ics, server_etag)) = self.get(&url)? else { return Ok(None) };
            // Someone changed this very occurrence since we saw it: their
            // change wins. (Changes to other occurrences don't matter.)
            if etag.is_some() && etag != server_etag.as_deref() && ical::has_override(&ics, key) {
                return Err(dav_error(412, "Precondition Failed: changed on the server".into()));
            }
            if !ical::remove_occurrence(&mut ics, key).map_err(|e| Unworkable(format!("{e:#}")))? {
                return self.delete(cal, href, server_etag.as_deref());
            }
            let pre = server_etag.as_deref().unwrap_or("*");
            let new_etag = self.put(&url, &ics, ("If-Match", pre))?;
            // Some servers leave the ETag out of a PUT's reply: ask.
            return match new_etag {
                Some(e) => Ok(Some(e)),
                None => Ok(self.get(&url)?.and_then(|(_, e)| e)),
            };
        }
        let headers: Vec<(&str, &str)> = etag.map(|e| vec![("If-Match", e)]).unwrap_or_default();
        let (_, resp) = self.send("DELETE", &url, &headers, None)?;
        if matches!(resp.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        Self::check(resp)?;
        Ok(None)
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn colors_and_addresses() {
        assert_eq!(parse_color("#FF2968FF"), Some([0xFF, 0x29, 0x68]));
        assert_eq!(parse_color("#1badf8"), Some([0x1B, 0xAD, 0xF8]));
        assert_eq!(parse_color("red"), None);
        assert_eq!(parse_color("#abcde€"), None, "8 bytes, but byte 6 is inside '€'");
    }

    #[test]
    fn redirects_only_carry_the_login_to_related_hosts() {
        let u = |s: &str| Url::parse(s).unwrap();
        let ok = |a: &str, b: &str| may_receive_login(&u(a), &u(b));
        assert!(ok("https://caldav.icloud.com/", "https://p42-caldav.icloud.com/x"), "iCloud's partition hosts");
        assert!(ok("https://dav.fastmail.com/", "https://dav.fastmail.com/dav/"));
        assert!(!ok("https://caldav.icloud.com/", "https://evil.example.com/"));
        assert!(!ok("https://icloud.com/", "https://evil.com/"), "parent of a two-label host is a TLD");
        assert!(!ok("https://dav.example.co.uk/", "https://evil.co.uk/"));
        assert!(!ok("http://127.0.0.1:8080/", "http://127.0.0.2/"));
        assert!(ok("http://127.0.0.1:8080/", "http://127.0.0.1:9090/"), "same host, other port");
    }

    #[test]
    fn events_go_inside_a_calendar_listed_without_a_trailing_slash() {
        let cal = CalRef::new("caldav-1", "https://dav.example.com/cal/work");
        let url = Client::resource_url(&Client::calendar_url(&cal).unwrap(), "abc.ics").unwrap();
        assert_eq!(url.as_str(), "https://dav.example.com/cal/work/abc.ics");
    }

    #[test]
    fn new_resource_ids_are_stable_across_retries() {
        assert_eq!(uid_for("local:0123abcd"), "0123abcd");
        // Moved or recreated events keep a non-local id; a retried create
        // must still land on the same resource.
        let moved = "caldav-1:https://h/cal/:/cal/a.ics";
        assert_eq!(uid_for(moved), uid_for(moved));
        assert_ne!(uid_for(moved), uid_for("caldav-1:https://h/cal/:/cal/b.ics"));
        assert_eq!(normalize_server("caldav.icloud.com").unwrap().as_str(), "https://caldav.icloud.com/");
        assert_eq!(normalize_server("http://127.0.0.1:5232/dav").unwrap().as_str(), "http://127.0.0.1:5232/dav/");
        assert!(normalize_server("http://").is_err());
        assert_eq!(utc_stamp(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap()), "20260501T000000Z");
    }

    #[test]
    fn sign_in_failures_are_explained() {
        assert!(explain(&dav_error(401, "Unauthorized".into())).unwrap().contains("app-specific password"));
        assert!(explain(&anyhow!("offline")).is_none());
    }
}
