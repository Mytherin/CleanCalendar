//! Sync bookkeeping, independent of the UI, the network, and of which
//! service (Google or CalDAV) an account is on.
//!
//! [`SyncEngine`] decides what to upload, when to retry, and how to merge
//! what a service sends back. It never does I/O itself: it queues [`Cmd`]s in
//! `outbox` for the worker thread, consumes the [`Msg`]s the worker returns,
//! and reports UI-visible effects as [`Notice`]s. That keeps every step
//! testable without a window, a thread, or a server.
//!
//! The engine changes the [`Store`] in memory; the caller saves it.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, NaiveDate};

use crate::dates::{add_months, first_of_month};
use crate::model::{CalDavAccount, Calendar, Event, EventFields, PendingDelete, Source, Store};
use crate::remote::{CalRef, CalendarChanges, Cmd, Failure, Msg, RemoteCalendar, GOOGLE};

/// Incremental syncs only report changes, so recurring-event occurrences
/// that slide into the sync window are picked up by a periodic full sync.
pub const FULL_SYNC_DAYS: i64 = 7;

/// The status shown while a service can't be reached.
pub fn offline(name: &str) -> String {
    format!("Offline — changes will sync when {name} is reachable")
}

/// Something the UI should react to.
#[derive(Debug, PartialEq)]
pub enum Notice {
    Status { text: String, error: bool },
    /// An event got a new id after its first upload.
    Renamed { from: String, to: String },
}

#[derive(Default)]
pub struct SyncEngine {
    /// Commands for the worker, in order.
    pub outbox: Vec<Cmd>,
    pub notices: Vec<Notice>,
    /// Uploads handed out and not answered yet ("save:<event id>",
    /// "del:<remote id>"), so retries don't queue duplicates.
    in_flight: HashSet<String>,
    /// Accounts with a sync out with the worker.
    running: HashSet<String>,
    /// Accounts asked to sync again while one was running.
    again: HashSet<String>,
    /// CalDAV accounts being added, until the server accepts the login.
    connecting: HashMap<String, CalDavAccount>,
}

impl SyncEngine {
    /// One-time fix-up for data written by older versions: events on synced
    /// calendars that were never uploaded become pending.
    pub fn prepare(store: &mut Store) {
        let remote_cals: HashSet<String> =
            store.calendars.iter().filter(|c| c.is_remote()).map(|c| c.id.clone()).collect();
        for ev in &mut store.events {
            if remote_cals.contains(&ev.calendar_id) && ev.remote_id.is_none() {
                ev.pending = true;
            }
        }
    }

    fn status(&mut self, text: impl Into<String>, error: bool) {
        self.notices.push(Notice::Status { text: text.into(), error });
    }

    /// Uploads waiting changes, then pulls remote changes, for every
    /// signed-in account. Returns whether any sync was queued.
    pub fn sync(&mut self, store: &Store, today: NaiveDate) -> bool {
        let mut any = false;
        for account in store.accounts() {
            any |= self.sync_account(store, &account, today);
        }
        any
    }

    /// Syncs one account (not when it's signed out or already syncing).
    pub fn sync_account(&mut self, store: &Store, account: &str, today: NaiveDate) -> bool {
        let Some(login) = store.login(account) else { return false };
        if self.running.contains(account) {
            self.again.insert(account.to_owned());
            return false;
        }
        // Uploads first: the worker runs commands in order, so the pull that
        // follows already sees them.
        self.retry_pending(store, account);

        let stale_before = today - Duration::days(FULL_SYNC_DAYS);
        let tokens: HashMap<String, String> = store
            .calendars
            .iter()
            .filter_map(|c| c.remote().filter(|r| r.account == account))
            .filter_map(|r| {
                let s = store.sync_state.get(&r.local_id())?;
                let fresh = s.full_sync_on.is_some_and(|d| d > stale_before);
                Some((r.id, s.token.clone().filter(|_| fresh)?))
            })
            .collect();
        let from = add_months(first_of_month(today), -6);
        let to = add_months(first_of_month(today), 13);
        self.running.insert(account.to_owned());
        self.outbox.push(Cmd::Sync { account: account.to_owned(), login, from, to, tokens });
        true
    }

    /// Re-sends every local change on this account not confirmed yet.
    fn retry_pending(&mut self, store: &Store, account: &str) {
        let on_account = |cal: Option<CalRef>| cal.is_some_and(|c| c.account == account);
        let ids: Vec<String> =
            store.events.iter().filter(|e| e.pending && on_account(store.remote_of(e))).map(|e| e.id.clone()).collect();
        for id in ids {
            self.push(store, &id);
        }
        for d in store.pending_deletes.clone() {
            if d.calendar.account == account {
                self.send_delete(store, d);
            }
        }
    }

    /// Call after the user changed an event (already updated in `store`):
    /// bumps its revision and uploads it if it lives on a service.
    pub fn event_changed(&mut self, store: &mut Store, id: &str) {
        let Some(ev) = store.event(id) else { return };
        let remote = store.remote_of(ev).is_some();
        if let Some(ev) = store.event_mut(id) {
            ev.rev += 1;
            ev.pending = remote;
        }
        self.push(store, id);
    }

    /// Call *before* changing an event: remembers the version the service
    /// confirmed, so a later conflict can be merged field by field.
    pub fn before_change(&mut self, store: &mut Store, id: &str) {
        let Some(ev) = store.event(id) else { return };
        if ev.base.is_none() && ev.remote_id.is_some() && store.remote_of(ev).is_some() {
            let fields = ev.fields();
            store.event_mut(id).unwrap().base = Some(fields);
        }
    }

    /// Call when an event leaves a synced calendar, with the event as it was
    /// before. `user_deleted`: the user deleted it, so the remote delete only
    /// applies if nobody changed it meanwhile (their change wins). Otherwise
    /// (moved to another calendar) the old copy is removed unconditionally.
    pub fn event_deleted(&mut self, store: &mut Store, ev: &Event, user_deleted: bool) {
        if let (Some(calendar), Some(remote_id)) = (store.remote_of(ev), ev.remote_id.clone()) {
            let etag = if user_deleted { ev.etag.clone() } else { None };
            self.queue_delete(store, PendingDelete { calendar, remote_id, etag });
        }
        // Never uploaded: if an upload is in flight, `on_saved` notices the
        // event is gone and deletes the copy it created.
    }

    /// Call when the user deleted a whole repeating series that the service
    /// sent as separate occurrences: deletes the series there.
    pub fn series_deleted(&mut self, store: &mut Store, calendar: CalRef, series_remote_id: &str) {
        self.queue_delete(store, PendingDelete { calendar, remote_id: series_remote_id.to_owned(), etag: None });
    }

    /// Uploads a pending event, unless an upload for it is already out
    /// (its reply triggers another upload if the event changed meanwhile).
    fn push(&mut self, store: &Store, id: &str) {
        let Some(ev) = store.event(id).filter(|e| e.pending).cloned() else { return };
        let Some(calendar) = store.remote_of(&ev) else { return };
        let Some(login) = store.login(&calendar.account) else { return };
        if self.in_flight.insert(format!("save:{id}")) {
            self.outbox.push(Cmd::Save { login, calendar, event: ev });
        }
    }

    fn send_delete(&mut self, store: &Store, d: PendingDelete) {
        let Some(login) = store.login(&d.calendar.account) else { return };
        if self.in_flight.insert(format!("del:{}", d.remote_id)) {
            self.outbox.push(Cmd::Delete { login, calendar: d.calendar, remote_id: d.remote_id, etag: d.etag });
        }
    }

    fn queue_delete(&mut self, store: &mut Store, d: PendingDelete) {
        if store.login(&d.calendar.account).is_none() {
            return;
        }
        if !store.pending_deletes.iter().any(|p| p.remote_id == d.remote_id) {
            store.pending_deletes.push(d.clone());
        }
        self.send_delete(store, d);
    }

    /// Forgets a calendar's sync token so the next sync re-downloads it,
    /// replacing local copies with what the service actually has.
    fn force_full_sync(&mut self, store: &mut Store, calendar: &CalRef, today: NaiveDate) {
        store.sync_state.remove(&calendar.local_id());
        self.sync_account(store, &calendar.account, today);
    }

    /// Starts adding a CalDAV account; it is kept once the server accepts
    /// the login (see [`Msg::Connected`]).
    pub fn connect_caldav(&mut self, account: CalDavAccount) {
        let Some(login) = account.login() else { return };
        self.outbox.push(Cmd::ConnectCalDav { account: account.id.clone(), login });
        self.connecting.insert(account.id.clone(), account);
    }

    /// Whether a CalDAV account is waiting for the server's answer.
    pub fn is_connecting(&self) -> bool {
        !self.connecting.is_empty()
    }

    /// Signs out of an account: drops its calendars, events, and sync state.
    pub fn disconnect(&mut self, store: &mut Store, account: &str) {
        let name = store.account_name(account);
        if account == GOOGLE {
            store.google.refresh_token = None;
            store.google.account_email = None;
        } else {
            store.caldav.retain(|a| a.id != account);
        }
        self.running.remove(account);
        self.again.remove(account);
        store.pending_deletes.retain(|d| d.calendar.account != account);
        let gone: HashSet<String> =
            store.calendars.iter().filter(|c| c.account() == Some(account)).map(|c| c.id.clone()).collect();
        store.calendars.retain(|c| !gone.contains(&c.id));
        store.events.retain(|e| !gone.contains(&e.calendar_id));
        store.sync_state.retain(|id, _| !gone.contains(id));
        self.status(format!("Disconnected from {name}"), false);
    }

    /// Processes one reply from the worker. `dragging` is the id of an event
    /// the user is dragging right now; a sync won't overwrite it.
    pub fn handle(&mut self, store: &mut Store, msg: Msg, dragging: Option<&str>, today: NaiveDate) {
        match msg {
            Msg::Connected { account, refresh_token } => {
                if account == GOOGLE {
                    store.google.refresh_token = refresh_token;
                } else if let Some(acct) = self.connecting.remove(&account) {
                    store.caldav.retain(|a| a.id != acct.id);
                    store.caldav.push(acct);
                } else {
                    return;
                }
                self.status(format!("Connected to {}", store.account_name(&account)), false);
                self.sync_account(store, &account, today);
            }
            Msg::ConnectFailed { account, error } => {
                self.connecting.remove(&account);
                self.status(error, true);
            }
            Msg::Synced { account, calendars, changes } => {
                self.running.remove(&account);
                // Signed out while the sync was out: nothing to apply it to.
                if store.login(&account).is_none() {
                    return;
                }
                merge_calendars(store, &account, &calendars);
                apply_changes(store, &account, changes, dragging, today);
                if store.unsynced_count() == 0 {
                    self.status("Up to date", false);
                }
                self.sync_finished(store, &account, today);
            }
            Msg::SyncFailed { account, error } => {
                self.running.remove(&account);
                if store.login(&account).is_some() {
                    self.status(format!("Couldn't sync with {}: {error}", store.account_name(&account)), true);
                }
                self.sync_finished(store, &account, today);
            }
            Msg::Saved { local_id, calendar, rev, remote_id, etag } => {
                self.on_saved(store, local_id, calendar, rev, remote_id, etag, today)
            }
            Msg::SaveFailed { local_id, calendar, error, kind } => {
                self.on_save_failed(store, local_id, calendar, error, kind, today)
            }
            Msg::SaveConflict { local_id, server } => {
                self.in_flight.remove(&format!("save:{local_id}"));
                self.resolve_save_conflict(store, &local_id, server);
            }
            Msg::DeleteConflict { remote_id, calendar, server } => {
                self.in_flight.remove(&format!("del:{remote_id}"));
                store.pending_deletes.retain(|d| d.remote_id != remote_id);
                // Their change beats our delete: bring the event back.
                if let Some(ev) = server {
                    if store.event(&ev.id).is_none() && store.calendar(&calendar.local_id()).is_some() {
                        self.status(format!("“{}” was changed on another device, so it wasn't deleted", ev.title), true);
                        store.events.push(ev);
                    }
                }
            }
            Msg::Deleted { remote_id } => {
                self.in_flight.remove(&format!("del:{remote_id}"));
                store.pending_deletes.retain(|d| d.remote_id != remote_id);
            }
            Msg::DeleteFailed { remote_id, calendar, error, kind } => {
                self.in_flight.remove(&format!("del:{remote_id}"));
                let name = store.account_name(&calendar.account);
                if kind == Failure::Retry {
                    self.status(offline(&name), true);
                } else {
                    // The service won't delete it: drop the request and show
                    // what it has.
                    store.pending_deletes.retain(|d| d.remote_id != remote_id);
                    self.status(format!("{name} refused a delete: {error}"), true);
                    self.force_full_sync(store, &calendar, today);
                }
            }
        }
    }

    fn sync_finished(&mut self, store: &Store, account: &str, today: NaiveDate) {
        if self.again.remove(account) {
            self.sync_account(store, account, today);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_saved(
        &mut self,
        store: &mut Store,
        local_id: String,
        calendar: CalRef,
        rev: u64,
        remote_id: String,
        etag: Option<String>,
        today: NaiveDate,
    ) {
        self.in_flight.remove(&format!("save:{local_id}"));
        let still_there = store.event(&local_id).is_some_and(|ev| store.remote_of(ev).as_ref() == Some(&calendar));
        if !still_there {
            // Deleted (or moved to another calendar) while the upload was in
            // flight: remove the copy that just landed there.
            self.queue_delete(store, PendingDelete { calendar, remote_id, etag: None });
            return;
        }
        let new_id = calendar.event_id(&remote_id);
        let ev = store.event_mut(&local_id).unwrap();
        ev.remote_id = Some(remote_id);
        ev.id = new_id.clone();
        ev.etag = etag;
        if ev.rev == rev {
            ev.pending = false;
            ev.base = None;
        } else {
            // Edited again meanwhile: what the service has now is what we
            // sent. (`base` stays the older snapshot; merging against it only
            // makes our side win more often, never loses their changes.)
        }
        // A new series: the service sends it back as occurrences, which
        // replace our copy (see `apply_changes`). Fetch them now.
        let new_series = ev.recurrence.is_some() && !ev.pending;
        let changed_again = ev.pending;
        if new_id != local_id {
            self.notices.push(Notice::Renamed { from: local_id, to: new_id.clone() });
        }
        if changed_again {
            self.push(store, &new_id);
        }
        if new_series {
            self.sync_account(store, &calendar.account, today);
        }
    }

    fn on_save_failed(
        &mut self,
        store: &mut Store,
        local_id: String,
        calendar: CalRef,
        error: String,
        kind: Failure,
        today: NaiveDate,
    ) {
        self.in_flight.remove(&format!("save:{local_id}"));
        let title = store.event(&local_id).map(|e| e.title.clone()).unwrap_or_default();
        let name = store.account_name(&calendar.account);
        match kind {
            // Stays pending; the next sync retries it.
            Failure::Retry => self.status(offline(&name), true),
            // Creating failed because the calendar itself is gone (updates of
            // deleted events arrive as conflicts instead), or it was refused.
            Failure::Gone | Failure::Rejected => {
                if let Some(ev) = store.event_mut(&local_id) {
                    ev.pending = false;
                    ev.base = None;
                }
                self.status(format!("{name} rejected changes to “{title}”: {error}"), true);
                self.force_full_sync(store, &calendar, today);
            }
        }
    }

    /// Our upload was refused because the event changed on the service
    /// first. Edits are never silently dropped: fields changed on only one
    /// side are combined, and if both sides changed the same field, ours
    /// wins. If the event was deleted there, our edited version is recreated.
    fn resolve_save_conflict(&mut self, store: &mut Store, local_id: &str, server: Option<Event>) {
        let Some(ev) = store.event_mut(local_id) else {
            // Deleted here meanwhile; that delete is queued separately.
            return;
        };
        let title = ev.title.clone();
        match server {
            None => {
                ev.remote_id = None;
                ev.recurring_id = None;
                ev.etag = None;
                ev.base = None;
                ev.pending = true;
                self.status(format!("“{title}” was deleted on another device; your edited version was kept"), true);
            }
            Some(srv) => {
                let theirs = srv.fields();
                let base = ev.base.clone().unwrap_or_else(|| theirs.clone());
                let (merged, clashes) = merge_fields(&base, &ev.fields(), &theirs);
                ev.set_fields(merged.clone());
                ev.etag = srv.etag;
                ev.recurring_id = srv.recurring_id;
                if merged == theirs {
                    // Nothing of ours left to send.
                    ev.pending = false;
                    ev.base = None;
                } else {
                    ev.base = Some(theirs);
                }
                let text = if clashes.is_empty() {
                    format!("Merged your changes to “{title}” with changes from another device")
                } else {
                    format!(
                        "“{title}” was also changed on another device; kept your {}",
                        clashes.join(" and ")
                    )
                };
                self.status(text, !clashes.is_empty());
            }
        }
        self.push(store, local_id);
    }
}

/// Three-way merge of event fields. A field changed on only one side takes
/// that side's value; changed differently on both sides, ours wins and the
/// field is reported as a clash. Start, end, all-day, time zone and
/// repeat rule move together.
fn merge_fields(base: &EventFields, ours: &EventFields, theirs: &EventFields) -> (EventFields, Vec<&'static str>) {
    let mut clashes = Vec::new();
    // Whether to take our value for one field (or field group).
    let mut take_ours = |name: &'static str, base, ours, theirs| {
        let ours_changed = ours != base;
        if ours_changed && theirs != base && ours != theirs {
            clashes.push(name);
        }
        ours_changed
    };
    let when = |f: &EventFields| format!("{} {} {} {:?} {:?}", f.start, f.end, f.all_day, f.recurrence, f.time_zone);
    let take_title = take_ours("title", base.title.clone(), ours.title.clone(), theirs.title.clone());
    let take_loc = take_ours("location", base.location.clone(), ours.location.clone(), theirs.location.clone());
    let take_notes = take_ours("notes", base.notes.clone(), ours.notes.clone(), theirs.notes.clone());
    let take_when = take_ours("time", when(base), when(ours), when(theirs));

    let from = |take_ours: bool| if take_ours { ours } else { theirs };
    let merged = EventFields {
        title: from(take_title).title.clone(),
        location: from(take_loc).location.clone(),
        notes: from(take_notes).notes.clone(),
        start: from(take_when).start,
        end: from(take_when).end,
        all_day: from(take_when).all_day,
        recurrence: from(take_when).recurrence.clone(),
        time_zone: from(take_when).time_zone.clone(),
    };
    (merged, clashes)
}

/// Where a calendar goes in the sidebar: local ones, then each account's in
/// account order.
fn account_rank(store: &Store, c: &Calendar) -> usize {
    match c.account() {
        None => 0,
        Some(GOOGLE) => 1,
        Some(a) => 2 + store.caldav.iter().position(|x| x.id == a).unwrap_or(usize::MAX - 3),
    }
}

/// Brings one account's calendar list in line with the service's.
fn merge_calendars(store: &mut Store, account: &str, remote: &[RemoteCalendar]) {
    if account == GOOGLE {
        store.google.account_email = remote.iter().find(|c| c.primary).map(|c| c.id.clone());
    }
    let refs: Vec<CalRef> = remote.iter().map(|c| CalRef::new(account, &c.id)).collect();
    let keep: HashSet<String> = refs.iter().map(CalRef::local_id).collect();
    store.calendars.retain(|c| c.account() != Some(account) || keep.contains(&c.id));
    for (rc, r) in remote.iter().zip(&refs) {
        let id = r.local_id();
        match store.calendars.iter_mut().find(|c| c.id == id) {
            // Name and permissions follow the service; color and visibility are the user's.
            Some(c) => {
                c.name = rc.name.clone();
                c.read_only = rc.read_only;
            }
            None => store.calendars.push(Calendar {
                id,
                name: rc.name.clone(),
                color: rc.color,
                visible: true,
                source: if account == GOOGLE {
                    Source::Google { calendar_id: rc.id.clone() }
                } else {
                    Source::CalDav { account: account.to_owned(), url: rc.id.clone() }
                },
                read_only: rc.read_only,
            }),
        }
    }
    // Local calendars first, then each account's; this account's in the
    // server's order, the others' as they were.
    let mut order: Vec<(usize, usize, usize)> = store
        .calendars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let pos = match c.remote() {
                Some(r) if r.account == account => refs.iter().position(|x| *x == r).unwrap_or(i),
                _ => i,
            };
            (account_rank(store, c), pos, i)
        })
        .collect();
    order.sort();
    let mut old: Vec<Option<Calendar>> = std::mem::take(&mut store.calendars).into_iter().map(Some).collect();
    store.calendars = order.into_iter().map(|(_, _, i)| old[i].take().unwrap()).collect();

    // Drop everything belonging to calendars that disappeared.
    let cal_ids: HashSet<String> = store.calendars.iter().map(|c| c.id.clone()).collect();
    store.events.retain(|e| cal_ids.contains(&e.calendar_id));
    store.sync_state.retain(|id, _| cal_ids.contains(id));
    store.pending_deletes.retain(|d| d.calendar.account != account || keep.contains(&d.calendar.local_id()));
}

/// Applies what a sync returned to the store. Events with unconfirmed local
/// edits (and the one being dragged) are left alone, and events waiting to be
/// deleted remotely are not brought back.
fn apply_changes(store: &mut Store, account: &str, changes: Vec<CalendarChanges>, dragging: Option<&str>, today: NaiveDate) {
    let deleting: HashSet<String> = store.pending_deletes.iter().map(|d| d.remote_id.clone()).collect();
    let protected = |e: &Event| e.pending || Some(e.id.as_str()) == dragging;

    for ch in changes {
        let cal_id = CalRef::new(account, &ch.calendar_id).local_id();
        if ch.full {
            store.events.retain(|e| e.calendar_id != cal_id || protected(e));
        } else if !ch.cancelled.is_empty() {
            // A cancelled id is either one event/occurrence or a whole series.
            let gone: HashSet<&str> = ch.cancelled.iter().map(String::as_str).collect();
            store.events.retain(|e| {
                let hit = e.calendar_id == cal_id
                    && (e.remote_id.as_deref().is_some_and(|r| gone.contains(r))
                        || e.recurring_id.as_deref().is_some_and(|r| gone.contains(r)));
                !hit || protected(e)
            });
        }
        let mut index: HashMap<String, usize> =
            store.events.iter().enumerate().map(|(i, e)| (e.id.clone(), i)).collect();
        for ev in ch.events {
            let being_deleted = |id: &Option<String>| id.as_ref().is_some_and(|r| deleting.contains(r));
            if being_deleted(&ev.remote_id) || being_deleted(&ev.recurring_id) {
                continue;
            }
            match index.get(&ev.id) {
                Some(&i) if protected(&store.events[i]) => {}
                Some(&i) => store.events[i] = ev,
                None => {
                    index.insert(ev.id.clone(), store.events.len());
                    store.events.push(ev);
                }
            }
        }
        // A series we uploaded comes back expanded into occurrences; once
        // those are here, our own copy of the series would show everything
        // twice.
        let expanded: HashSet<String> = store
            .events
            .iter()
            .filter(|e| e.calendar_id == cal_id)
            .filter_map(|e| e.recurring_id.clone())
            .collect();
        store.events.retain(|e| {
            let superseded = e.calendar_id == cal_id
                && e.recurrence.is_some()
                && e.remote_id.as_ref().is_some_and(|r| expanded.contains(r));
            !superseded || protected(e)
        });
        let state = store.sync_state.entry(cal_id).or_default();
        state.token = ch.next_token;
        if ch.full {
            state.full_sync_on = Some(today);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dates::midnight;
    use crate::google;
    use crate::model::SyncState;

    const CAL: &str = "me@example.com";

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()
    }

    fn gev(rid: &str, title: &str) -> Event {
        let start = midnight(today()) + Duration::hours(9);
        Event {
            id: google::local_event_id(CAL, rid),
            calendar_id: google::local_calendar_id(CAL),
            title: title.into(),
            start,
            end: start + Duration::hours(1),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            remote_id: Some(rid.into()),
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
            recurrence: None,
            time_zone: None,
        }
    }

    /// An event created locally on the Google calendar, not uploaded yet.
    fn new_gev(local: &str, title: &str) -> Event {
        Event { id: local.into(), remote_id: None, ..gev("unused", title) }
    }

    fn remote_cal() -> RemoteCalendar {
        RemoteCalendar { id: CAL.into(), name: "Me".into(), color: [0, 0, 255], read_only: false, primary: true }
    }

    /// A signed-in store with one Google calendar.
    fn store_with(events: Vec<Event>) -> Store {
        let mut store = Store::default();
        store.google.refresh_token = Some("refresh".into());
        merge_calendars(&mut store, GOOGLE, &[remote_cal()]);
        store.events = events;
        store
    }

    fn changes(full: bool, events: Vec<Event>, cancelled: &[&str], token: Option<&str>) -> Vec<CalendarChanges> {
        vec![CalendarChanges {
            calendar_id: CAL.into(),
            full,
            events,
            cancelled: cancelled.iter().map(|s| s.to_string()).collect(),
            next_token: token.map(str::to_owned),
        }]
    }

    fn titles(store: &Store) -> Vec<String> {
        let mut t: Vec<String> = store.events.iter().map(|e| e.title.clone()).collect();
        t.sort();
        t
    }

    /// Commands queued since the last call, in a comparable form.
    #[derive(Debug, PartialEq)]
    enum Sent {
        Save { id: String, rev: u64, remote_id: Option<String> },
        Delete(String),
        Sync(Vec<String>),
    }

    fn sent(engine: &mut SyncEngine) -> Vec<Sent> {
        engine
            .outbox
            .drain(..)
            .map(|cmd| match cmd {
                Cmd::Save { event, .. } => Sent::Save { id: event.id, rev: event.rev, remote_id: event.remote_id },
                Cmd::Delete { remote_id, .. } => Sent::Delete(remote_id),
                Cmd::Sync { tokens, .. } => {
                    let mut cals: Vec<String> = tokens.into_keys().collect();
                    cals.sort();
                    Sent::Sync(cals)
                }
                c => panic!("unexpected {c:?}"),
            })
            .collect()
    }

    fn save(id: &str, rev: u64, remote_id: Option<&str>) -> Sent {
        Sent::Save { id: id.into(), rev, remote_id: remote_id.map(str::to_owned) }
    }

    fn saved(local_id: &str, rev: u64, remote_id: &str) -> Msg {
        Msg::Saved {
            local_id: local_id.into(),
            calendar: CalRef::google(CAL),
            rev,
            remote_id: remote_id.into(),
            etag: Some(format!("etag-{rev}")),
        }
    }

    fn save_failed(local_id: &str, kind: Failure) -> Msg {
        Msg::SaveFailed { local_id: local_id.into(), calendar: CalRef::google(CAL), error: "nope".into(), kind }
    }

    fn synced(ch: Vec<CalendarChanges>) -> Msg {
        Msg::Synced { account: GOOGLE.into(), calendars: vec![remote_cal()], changes: ch }
    }

    fn edit(store: &mut Store, engine: &mut SyncEngine, id: &str, title: &str) {
        engine.before_change(store, id);
        store.event_mut(id).unwrap().title = title.into();
        engine.event_changed(store, id);
    }

    // ---- uploads ---------------------------------------------------------

    #[test]
    fn edit_is_uploaded_and_confirmed() {
        let id = google::local_event_id(CAL, "a");
        let mut store = store_with(vec![gev("a", "old")]);
        let mut engine = SyncEngine::default();

        edit(&mut store, &mut engine, &id, "new");
        assert_eq!(sent(&mut engine), [save(&id, 1, Some("a"))]);
        assert!(store.event(&id).unwrap().pending);

        engine.handle(&mut store, saved(&id, 1, "a"), None, today());
        assert!(!store.event(&id).unwrap().pending);
        assert_eq!(store.unsynced_count(), 0);
    }

    #[test]
    fn offline_edit_is_kept_and_retried_on_next_sync() {
        let id = google::local_event_id(CAL, "a");
        let mut store = store_with(vec![gev("a", "old")]);
        let mut engine = SyncEngine::default();

        edit(&mut store, &mut engine, &id, "new");
        sent(&mut engine);
        engine.handle(&mut store, save_failed(&id, Failure::Retry), None, today());
        assert!(store.event(&id).unwrap().pending);
        assert!(engine.notices.contains(&Notice::Status { text: offline("Google"), error: true }));

        // The next sync re-sends the edit before pulling.
        assert!(engine.sync(&store, today()));
        assert_eq!(sent(&mut engine), [save(&id, 1, Some("a")), Sent::Sync(vec![])]);

        // A sync result that still has the old title doesn't undo the edit.
        engine.handle(&mut store, synced(changes(true, vec![gev("a", "old")], &[], None)), None, today());
        assert_eq!(titles(&store), ["new"]);

        engine.handle(&mut store, saved(&id, 1, "a"), None, today());
        assert_eq!(store.unsynced_count(), 0);
    }

    #[test]
    fn edit_during_upload_is_sent_after_it() {
        let id = google::local_event_id(CAL, "a");
        let mut store = store_with(vec![gev("a", "v0")]);
        let mut engine = SyncEngine::default();

        edit(&mut store, &mut engine, &id, "v1");
        edit(&mut store, &mut engine, &id, "v2");
        // Only one upload at a time per event.
        assert_eq!(sent(&mut engine), [save(&id, 1, Some("a"))]);

        // Confirming v1 doesn't clear v2; v2 goes out next.
        engine.handle(&mut store, saved(&id, 1, "a"), None, today());
        assert!(store.event(&id).unwrap().pending);
        assert_eq!(sent(&mut engine), [save(&id, 2, Some("a"))]);

        engine.handle(&mut store, saved(&id, 2, "a"), None, today());
        assert!(!store.event(&id).unwrap().pending);
        assert!(sent(&mut engine).is_empty());
    }

    #[test]
    fn new_event_takes_its_google_id() {
        let mut store = store_with(vec![new_gev("local:x", "lunch")]);
        let mut engine = SyncEngine::default();

        engine.event_changed(&mut store, "local:x");
        assert_eq!(sent(&mut engine), [save("local:x", 1, None)]);

        engine.handle(&mut store, saved("local:x", 1, "g1"), None, today());
        let new_id = google::local_event_id(CAL, "g1");
        let ev = store.event(&new_id).expect("renamed");
        assert_eq!(ev.remote_id.as_deref(), Some("g1"));
        assert!(!ev.pending);
        assert!(engine.notices.contains(&Notice::Renamed { from: "local:x".into(), to: new_id }));
    }

    #[test]
    fn local_events_are_never_uploaded() {
        let mut ev = new_gev("local:y", "private");
        ev.calendar_id = "local:home".into();
        let mut store = store_with(vec![ev]);
        let mut engine = SyncEngine::default();

        engine.event_changed(&mut store, "local:y");
        assert!(sent(&mut engine).is_empty());
        assert!(!store.event("local:y").unwrap().pending);
    }

    #[test]
    fn signed_out_queues_nothing() {
        let mut store = store_with(vec![gev("a", "x")]);
        store.google.refresh_token = None;
        let mut engine = SyncEngine::default();

        engine.event_changed(&mut store, &google::local_event_id(CAL, "a"));
        assert!(!engine.sync(&store, today()));
        assert!(sent(&mut engine).is_empty());
        // Still marked, so it uploads once signed in.
        assert_eq!(store.unsynced_count(), 1);
    }

    // ---- deletes ---------------------------------------------------------

    #[test]
    fn delete_is_queued_retried_and_confirmed() {
        let ev = gev("a", "x");
        let mut store = store_with(vec![ev.clone()]);
        let mut engine = SyncEngine::default();

        store.events.clear();
        engine.event_deleted(&mut store, &ev, true);
        assert_eq!(sent(&mut engine), [Sent::Delete("a".into())]);

        let failed = Msg::DeleteFailed {
            remote_id: "a".into(),
            calendar: CalRef::google(CAL),
            error: "timeout".into(),
            kind: Failure::Retry,
        };
        engine.handle(&mut store, failed, None, today());
        assert_eq!(store.pending_deletes.len(), 1);

        // While waiting, a sync doesn't bring the event back.
        engine.sync(&store, today());
        assert_eq!(sent(&mut engine), [Sent::Delete("a".into()), Sent::Sync(vec![])]);
        engine.handle(&mut store, synced(changes(true, vec![gev("a", "x")], &[], None)), None, today());
        assert!(store.events.is_empty());

        engine.handle(&mut store, Msg::Deleted { remote_id: "a".into() }, None, today());
        assert!(store.pending_deletes.is_empty());
    }

    #[test]
    fn deleting_before_first_upload_finishes_removes_the_remote_copy() {
        let mut store = store_with(vec![new_gev("local:x", "oops")]);
        let mut engine = SyncEngine::default();

        engine.event_changed(&mut store, "local:x");
        sent(&mut engine);
        let ev = store.event("local:x").unwrap().clone();
        store.events.clear();
        engine.event_deleted(&mut store, &ev, true);
        assert!(sent(&mut engine).is_empty(), "nothing to delete remotely yet");

        // The upload lands after all: delete what it created.
        engine.handle(&mut store, saved("local:x", 1, "g1"), None, today());
        assert_eq!(sent(&mut engine), [Sent::Delete("g1".into())]);
        assert!(store.events.is_empty());
    }

    #[test]
    fn refused_delete_restores_googles_version() {
        let ev = gev("a", "x");
        let mut store = store_with(vec![]);
        store.sync_state.insert(google::local_calendar_id(CAL), SyncState { token: Some("t".into()), full_sync_on: Some(today()) });
        let mut engine = SyncEngine::default();
        engine.event_deleted(&mut store, &ev, true);
        sent(&mut engine);

        let refused = Msg::DeleteFailed {
            remote_id: "a".into(),
            calendar: CalRef::google(CAL),
            error: "forbidden".into(),
            kind: Failure::Rejected,
        };
        engine.handle(&mut store, refused, None, today());
        assert!(store.pending_deletes.is_empty());
        // Full sync of that calendar (no token sent for it).
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![])]);
    }

    // ---- failures --------------------------------------------------------

    // ---- conflicts -------------------------------------------------------

    /// Google's copy of event "a" after another device changed it.
    fn theirs(change: impl FnOnce(&mut Event)) -> Event {
        let mut ev = gev("a", "Standup");
        ev.etag = Some("theirs".into());
        change(&mut ev);
        ev
    }

    fn conflict(server: Option<Event>) -> Msg {
        Msg::SaveConflict { local_id: google::local_event_id(CAL, "a"), server }
    }

    fn statuses(engine: &SyncEngine) -> Vec<String> {
        engine
            .notices
            .iter()
            .filter_map(|n| match n {
                Notice::Status { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Store with event "a" (title "Standup", 9:00–10:00) that we've edited
    /// locally with `ours`; the upload is out.
    fn edited_locally(ours: impl FnOnce(&mut Event)) -> (Store, SyncEngine, String) {
        let id = google::local_event_id(CAL, "a");
        let mut original = gev("a", "Standup");
        original.etag = Some("v1".into());
        let mut store = store_with(vec![original]);
        let mut engine = SyncEngine::default();
        engine.before_change(&mut store, &id);
        ours(store.event_mut(&id).unwrap());
        engine.event_changed(&mut store, &id);
        sent(&mut engine);
        (store, engine, id)
    }

    #[test]
    fn uploads_carry_the_version_they_were_based_on() {
        let id = google::local_event_id(CAL, "a");
        let mut ev = gev("a", "Standup");
        ev.etag = Some("v1".into());
        let mut store = store_with(vec![ev]);
        let mut engine = SyncEngine::default();

        edit(&mut store, &mut engine, &id, "Daily standup");
        match &engine.outbox[0] {
            Cmd::Save { event, .. } => assert_eq!(event.etag.as_deref(), Some("v1")),
            _ => panic!("expected a save"),
        }
        assert_eq!(store.event(&id).unwrap().base.as_ref().unwrap().title, "Standup", "snapshot taken");

        engine.handle(&mut store, saved(&id, 1, "a"), None, today());
        let ev = store.event(&id).unwrap();
        assert_eq!(ev.etag.as_deref(), Some("etag-1"), "new version remembered");
        assert_eq!(ev.base, None, "snapshot dropped once confirmed");
    }

    #[test]
    fn concurrent_edits_to_different_fields_are_merged() {
        // We moved it to 11:00; the other device renamed it.
        let (mut store, mut engine, id) = edited_locally(|e| {
            e.start += Duration::hours(2);
            e.end += Duration::hours(2);
        });
        engine.handle(&mut store, conflict(Some(theirs(|e| e.title = "Standup (room 2)".into()))), None, today());

        let ev = store.event(&id).unwrap();
        assert_eq!(ev.title, "Standup (room 2)", "their rename kept");
        assert_eq!(ev.start.time(), chrono::NaiveTime::from_hms_opt(11, 0, 0).unwrap(), "our move kept");
        assert!(ev.pending);
        assert_eq!(ev.etag.as_deref(), Some("theirs"), "next upload is based on their version");
        assert_eq!(sent(&mut engine), [save(&id, 1, Some("a"))], "merged result uploaded");
        assert!(statuses(&engine)[0].starts_with("Merged your changes"));
    }

    #[test]
    fn concurrent_edits_to_the_same_field_keep_ours_and_say_so() {
        let (mut store, mut engine, id) = edited_locally(|e| e.title = "Ours".into());
        engine.handle(
            &mut store,
            conflict(Some(theirs(|e| {
                e.title = "Theirs".into();
                e.location = "Room 9".into();
            }))),
            None,
            today(),
        );
        let ev = store.event(&id).unwrap();
        assert_eq!((ev.title.as_str(), ev.location.as_str()), ("Ours", "Room 9"));
        assert_eq!(sent(&mut engine).len(), 1);
        let status = &statuses(&engine)[0];
        assert!(status.contains("kept your title"), "{status}");
    }

    #[test]
    fn identical_concurrent_edits_need_no_upload() {
        let (mut store, mut engine, id) = edited_locally(|e| e.title = "Renamed".into());
        engine.handle(&mut store, conflict(Some(theirs(|e| e.title = "Renamed".into()))), None, today());
        let ev = store.event(&id).unwrap();
        assert!(!ev.pending);
        assert_eq!(ev.base, None);
        assert!(sent(&mut engine).is_empty());
    }

    #[test]
    fn edit_beats_delete_on_another_device() {
        let (mut store, mut engine, id) = edited_locally(|e| e.title = "Still needed".into());
        engine.handle(&mut store, conflict(None), None, today());

        let ev = store.event(&id).unwrap();
        assert_eq!((ev.title.as_str(), ev.remote_id.as_deref()), ("Still needed", None));
        // Recreated on Google.
        assert_eq!(sent(&mut engine), [save(&id, 1, None)]);
        assert!(statuses(&engine)[0].contains("your edited version was kept"));
    }

    #[test]
    fn edit_on_another_device_beats_our_delete() {
        let mut ev = gev("a", "Standup");
        ev.etag = Some("v1".into());
        let mut store = store_with(vec![]);
        let mut engine = SyncEngine::default();
        engine.event_deleted(&mut store, &ev, true);
        match &engine.outbox[0] {
            Cmd::Delete { etag, .. } => assert_eq!(etag.as_deref(), Some("v1"), "delete only that version"),
            _ => panic!("expected a delete"),
        }
        sent(&mut engine);

        let msg = Msg::DeleteConflict {
            remote_id: "a".into(),
            calendar: CalRef::google(CAL),
            server: Some(theirs(|e| e.title = "Standup, moved".into())),
        };
        engine.handle(&mut store, msg, None, today());
        assert_eq!(titles(&store), ["Standup, moved"], "restored with their change");
        assert!(store.pending_deletes.is_empty());
        assert!(statuses(&engine)[0].contains("wasn't deleted"));
    }

    #[test]
    fn moving_to_another_calendar_removes_the_old_copy_unconditionally() {
        let mut ev = gev("a", "Standup");
        ev.etag = Some("v1".into());
        let mut store = store_with(vec![]);
        let mut engine = SyncEngine::default();
        engine.event_deleted(&mut store, &ev, false);
        match &engine.outbox[0] {
            Cmd::Delete { etag, .. } => assert_eq!(*etag, None),
            _ => panic!("expected a delete"),
        }
    }

    #[test]
    fn a_sync_during_a_conflict_does_not_lose_our_edit() {
        let (mut store, mut engine, id) = edited_locally(|e| e.notes = "bring slides".into());
        // A pull arrives with their newer copy before our upload is answered.
        let theirs_now = theirs(|e| e.title = "Standup v2".into());
        engine.handle(&mut store, synced(changes(true, vec![theirs_now.clone()], &[], None)), None, today());
        assert_eq!(store.event(&id).unwrap().notes, "bring slides", "pending edit kept");
        // Then the upload is refused, and the merge combines both.
        engine.handle(&mut store, conflict(Some(theirs_now)), None, today());
        let ev = store.event(&id).unwrap();
        assert_eq!((ev.title.as_str(), ev.notes.as_str()), ("Standup v2", "bring slides"));
    }

    #[test]
    fn merge_rules() {
        let base = gev("a", "T").fields();
        let with = |f: fn(&mut EventFields)| {
            let mut x = base.clone();
            f(&mut x);
            x
        };
        let ours = with(|f| f.notes = "ours".into());
        let theirs = with(|f| f.location = "theirs".into());
        let (m, clashes) = merge_fields(&base, &ours, &theirs);
        assert_eq!((m.notes.as_str(), m.location.as_str()), ("ours", "theirs"));
        assert!(clashes.is_empty());

        // Time fields move as one: their all-day switch vs our new start.
        let ours = with(|f| f.start += Duration::minutes(30));
        let theirs = with(|f| f.all_day = true);
        let (m, clashes) = merge_fields(&base, &ours, &theirs);
        assert_eq!((m.start, m.all_day), (ours.start, false));
        assert_eq!(clashes, ["time"]);
    }

    #[test]
    fn rejected_edit_falls_back_to_googles_version() {
        let id = google::local_event_id(CAL, "a");
        let mut store = store_with(vec![gev("a", "x")]);
        store.sync_state.insert(google::local_calendar_id(CAL), SyncState { token: Some("t".into()), full_sync_on: Some(today()) });
        let mut engine = SyncEngine::default();

        edit(&mut store, &mut engine, &id, "y");
        sent(&mut engine);
        engine.handle(&mut store, save_failed(&id, Failure::Rejected), None, today());
        assert!(!store.event(&id).unwrap().pending);
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![])], "full sync, no token");

        engine.handle(&mut store, synced(changes(true, vec![gev("a", "x")], &[], Some("t2"))), None, today());
        assert_eq!(titles(&store), ["x"]);
    }

    // ---- pulling ---------------------------------------------------------

    #[test]
    fn overlapping_sync_requests_run_one_after_another() {
        let mut store = store_with(vec![]);
        let mut engine = SyncEngine::default();

        assert!(engine.sync(&store, today()));
        assert!(!engine.sync(&store, today()));
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![])]);

        engine.handle(&mut store, synced(changes(true, vec![], &[], Some("t1"))), None, today());
        // The second request runs now, incrementally.
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![CAL.into()])]);
        engine.handle(&mut store, Msg::SyncFailed { account: GOOGLE.into(), error: "offline".into() }, None, today());
        assert!(engine.sync(&store, today()), "a failed sync doesn't block the next one");
    }

    #[test]
    fn full_sync_is_forced_weekly() {
        let mut store = store_with(vec![]);
        let mut engine = SyncEngine::default();
        let state = |days_ago| SyncState { token: Some("t".into()), full_sync_on: Some(today() - Duration::days(days_ago)) };

        store.sync_state.insert(google::local_calendar_id(CAL), state(FULL_SYNC_DAYS - 1));
        engine.sync(&store, today());
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![CAL.into()])]);
        engine.handle(&mut store, Msg::SyncFailed { account: GOOGLE.into(), error: "x".into() }, None, today());

        store.sync_state.insert(google::local_calendar_id(CAL), state(FULL_SYNC_DAYS));
        engine.sync(&store, today());
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![])]);
    }

    #[test]
    fn full_sync_replaces_but_keeps_pending_edits() {
        let mut edited = gev("a", "my edit");
        edited.pending = true;
        let mut not_uploaded = new_gev("local:x", "not uploaded");
        not_uploaded.pending = true;
        let mut store = store_with(vec![edited, gev("b", "stale"), not_uploaded]);

        apply_changes(
            &mut store,
            GOOGLE,
            changes(true, vec![gev("a", "server"), gev("c", "new remote")], &[], Some("t1")),
            None,
            today(),
        );
        assert_eq!(titles(&store), ["my edit", "new remote", "not uploaded"]);
        let st = &store.sync_state[&google::local_calendar_id(CAL)];
        assert_eq!(st.token.as_deref(), Some("t1"));
        assert_eq!(st.full_sync_on, Some(today()));
    }

    #[test]
    fn incremental_sync_upserts_and_cancels() {
        let mut occurrence = gev("series_20261114", "standup");
        occurrence.recurring_id = Some("series".into());
        let mut store = store_with(vec![gev("a", "old title"), gev("b", "untouched"), gev("c", "deleted"), occurrence]);

        apply_changes(
            &mut store,
            GOOGLE,
            changes(false, vec![gev("a", "new title"), gev("d", "added")], &["c", "series"], Some("t2")),
            None,
            today(),
        );
        assert_eq!(titles(&store), ["added", "new title", "untouched"]);
        // Incremental syncs don't reset the full-sync clock.
        assert_eq!(store.sync_state[&google::local_calendar_id(CAL)].full_sync_on, None);
    }

    #[test]
    fn dragged_event_is_not_overwritten() {
        let mut store = store_with(vec![gev("drag", "being dragged")]);
        apply_changes(
            &mut store,
            GOOGLE,
            changes(true, vec![gev("drag", "server copy")], &[], None),
            Some(&google::local_event_id(CAL, "drag")),
            today(),
        );
        assert_eq!(titles(&store), ["being dragged"]);
        // No token means the next sync is a full one again.
        assert_eq!(store.sync_state[&google::local_calendar_id(CAL)].token, None);
    }

    #[test]
    fn removed_calendars_take_their_events_along() {
        let mut store = store_with(vec![gev("a", "x")]);
        store.sync_state.insert(google::local_calendar_id(CAL), SyncState::default());
        let mut engine = SyncEngine::default();
        engine.sync(&store, today());

        engine.handle(&mut store, Msg::Synced { account: GOOGLE.into(), calendars: vec![], changes: vec![] }, None, today());
        assert!(store.calendars.iter().all(|c| !c.is_remote()));
        assert!(store.events.is_empty());
        assert!(store.sync_state.is_empty());
    }

    #[test]
    fn calendar_color_and_visibility_are_kept() {
        let mut store = store_with(vec![]);
        let cal = store.calendars.iter_mut().find(|c| c.remote() == Some(CalRef::google(CAL))).unwrap();
        cal.color = [1, 2, 3];
        cal.visible = false;
        let mut renamed = remote_cal();
        renamed.name = "Renamed".into();
        merge_calendars(&mut store, GOOGLE, &[renamed]);
        let cal = store.calendars.iter().find(|c| c.remote() == Some(CalRef::google(CAL))).unwrap();
        assert_eq!((cal.name.as_str(), cal.color, cal.visible), ("Renamed", [1, 2, 3], false));
    }

    #[test]
    fn old_unuploaded_events_become_pending() {
        let mut store = store_with(vec![new_gev("local:x", "old version")]);
        SyncEngine::prepare(&mut store);
        assert_eq!(store.unsynced_count(), 1);
    }

    // ---- Repeating events and time zones ----------------------------------

    use crate::recurrence::{Freq, Recurrence};

    #[test]
    fn a_new_series_is_replaced_by_googles_occurrences() {
        let mut series = new_gev("local:s", "standup");
        series.recurrence = Some(Recurrence::new(Freq::Weekly));
        let mut store = store_with(vec![series]);
        let mut engine = SyncEngine::default();
        engine.event_changed(&mut store, "local:s");
        assert_eq!(sent(&mut engine), [save("local:s", 1, None)]);

        // Confirmed: fetch Google's occurrences right away. Until they
        // arrive, our copy of the series is what shows.
        engine.handle(&mut store, saved("local:s", 1, "s"), None, today());
        assert_eq!(sent(&mut engine), [Sent::Sync(vec![])]);
        assert_eq!(store.events.len(), 1);

        let mut occ = gev("s_20261114T080000Z", "standup");
        occ.recurring_id = Some("s".into());
        engine.handle(&mut store, synced(changes(false, vec![occ], &[], Some("t"))), None, today());
        assert_eq!(store.events.len(), 1, "series and occurrence would show twice");
        assert_eq!(store.events[0].recurring_id.as_deref(), Some("s"));
        assert!(store.events[0].recurrence.is_none());
    }

    #[test]
    fn a_series_with_unsent_edits_is_kept_until_they_are_sent() {
        let mut series = gev("s", "standup v2");
        series.recurrence = Some(Recurrence::new(Freq::Daily));
        series.pending = true;
        let mut occ = gev("s_1", "standup");
        occ.recurring_id = Some("s".into());
        let mut store = store_with(vec![series]);
        apply_changes(&mut store, GOOGLE, changes(false, vec![occ], &[], None), None, today());
        assert_eq!(titles(&store), ["standup", "standup v2"]);
    }

    #[test]
    fn deleting_a_google_series_deletes_it_once_and_keeps_it_gone() {
        let mut store = store_with(vec![]);
        let mut engine = SyncEngine::default();
        engine.series_deleted(&mut store, CalRef::google(CAL), "s");
        assert_eq!(sent(&mut engine), [Sent::Delete("s".into())]);
        assert_eq!(store.unsynced_count(), 1);

        // Its occurrences in a sync that crossed the delete stay away.
        let mut occ = gev("s_1", "standup");
        occ.recurring_id = Some("s".into());
        apply_changes(&mut store, GOOGLE, changes(false, vec![occ], &[], None), None, today());
        assert!(store.events.is_empty());

        engine.handle(&mut store, Msg::Deleted { remote_id: "s".into() }, None, today());
        assert_eq!(store.unsynced_count(), 0);
    }

    #[test]
    fn time_zone_and_repeat_rule_merge_as_part_of_the_time() {
        let base = gev("a", "Standup").fields();
        let mut ours = base.clone();
        ours.title = "Daily standup".into();
        let mut theirs = base.clone();
        theirs.time_zone = Some("Europe/Paris".into());
        theirs.recurrence = Some(Recurrence::new(Freq::Daily));
        let (merged, clashes) = merge_fields(&base, &ours, &theirs);
        assert!(clashes.is_empty());
        assert_eq!(merged.title, "Daily standup");
        assert_eq!(merged.time_zone.as_deref(), Some("Europe/Paris"));
        assert!(merged.recurrence.is_some());

        // Both changed when it happens: ours wins, and that's a clash.
        let mut ours = base.clone();
        ours.time_zone = Some("Asia/Tokyo".into());
        let (merged, clashes) = merge_fields(&base, &ours, &theirs);
        assert_eq!(clashes, ["time"]);
        assert_eq!((merged.time_zone.as_deref(), merged.recurrence), (Some("Asia/Tokyo"), None));
    }
}
