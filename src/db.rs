//! On-disk storage, split by who owns the data:
//!
//! - `data.db` (persistent, in the user's data folder): Google sign-in
//!   settings, the calendar list with the user's colours and visibility,
//!   events on local calendars, and local changes to Google events that
//!   haven't been uploaded yet (`outbox`, `pending_deletes`). None of this
//!   can be rebuilt from Google, so it is never thrown away.
//! - `cache.db` (disposable, in the user's cache folder): Google events as
//!   last downloaded, and the incremental-sync tokens. Google is the source
//!   of truth here; if the file is deleted or unreadable it is started over
//!   and the next sync downloads everything again.
//!
//! The cache is ATTACHed to the same connection, so one save commits to both
//! files atomically (an event whose upload was just confirmed moves from the
//! outbox to the cache in a single transaction).
//!
//! The app keeps working on the in-memory [`Store`]; [`Db::save`] compares it
//! with what was last written and only writes the rows that changed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, Row, Transaction};

use crate::model::{Calendar, Event, GoogleSettings, PendingDelete, Source, Store, SyncState};

const DATA_VERSION: i32 = 1;
const CACHE_VERSION: i32 = 1;

/// Where the two databases (and the old JSON store, for migration) live.
#[derive(Clone, Debug)]
pub struct Paths {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub legacy_json: PathBuf,
}

impl Paths {
    /// `CLEANCALENDAR_DATA=<dir>` puts everything in one folder; otherwise the
    /// OS's per-user data and cache folders are used.
    pub fn default_location() -> Self {
        if let Some(dir) = std::env::var_os("CLEANCALENDAR_DATA") {
            return Paths::in_dir(Path::new(&dir));
        }
        match directories::ProjectDirs::from("com", "CleanCalendar", "CleanCalendar") {
            Some(d) => Paths {
                data: d.data_dir().join("data.db"),
                cache: d.cache_dir().join("cache.db"),
                legacy_json: d.data_dir().join("store.json"),
            },
            None => Paths::in_dir(Path::new(".")),
        }
    }

    pub fn in_dir(dir: &Path) -> Self {
        Paths { data: dir.join("data.db"), cache: dir.join("cache.db"), legacy_json: dir.join("store.json") }
    }
}

/// Which table an event is stored in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// On a local calendar: `main.events`.
    Local,
    /// On a Google calendar with unconfirmed local changes: `main.outbox`.
    Outbox,
    /// On a Google calendar, as the server has it: `cache.events`.
    Cache,
}

impl Place {
    fn table(self) -> &'static str {
        match self {
            Place::Local => "main.events",
            Place::Outbox => "main.outbox",
            Place::Cache => "cache.events",
        }
    }
}

/// What the databases currently hold, to work out what a save must write.
#[derive(Debug, Default)]
struct Saved {
    calendars: Vec<Calendar>,
    google: GoogleSettings,
    pending_deletes: Vec<PendingDelete>,
    sync_state: HashMap<String, SyncState>,
    events: HashMap<String, (Place, Event)>,
}

#[derive(Debug)]
pub struct Db {
    conn: Connection,
    saved: Saved,
}

/// `data.db` was written by a newer version of the app; leave it alone.
#[derive(Debug)]
struct TooNew(i32);

impl std::fmt::Display for TooNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "written by a newer version of the app (schema {})", self.0)
    }
}

impl std::error::Error for TooNew {}

const EVENT_COLUMNS: &str = "id, calendar_id, title, start_at, end_at, all_day, location, notes, \
                             remote_id, recurring_id, pending, rev, etag, base";

fn event_table(name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
            id TEXT PRIMARY KEY,
            calendar_id TEXT NOT NULL,
            title TEXT NOT NULL,
            start_at TEXT NOT NULL,
            end_at TEXT NOT NULL,
            all_day INTEGER NOT NULL,
            location TEXT NOT NULL,
            notes TEXT NOT NULL,
            remote_id TEXT,
            recurring_id TEXT,
            pending INTEGER NOT NULL,
            rev INTEGER NOT NULL,
            etag TEXT,
            base TEXT
        );"
    )
}

fn user_version(conn: &Connection, schema: &str) -> rusqlite::Result<i32> {
    conn.query_row(&format!("PRAGMA {schema}.user_version"), [], |r| r.get(0))
}

/// Opens both databases and loads the store. Never fails: an unreadable
/// `data.db` is moved aside (like the old JSON store was), an unreadable
/// cache is started over, and if nothing can be opened the app runs without
/// saving.
pub fn open(paths: &Paths) -> Store {
    for p in [&paths.data, &paths.cache] {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
    }

    let fresh = !paths.data.exists();
    let (conn, data) = match open_data(&paths.data) {
        Ok(x) => x,
        Err(e) if e.is::<TooNew>() => {
            eprintln!("Not opening {}: {e}. Changes will not be saved.", paths.data.display());
            return Store::with_defaults();
        }
        Err(e) => {
            eprintln!("Could not read {}: {e:#}", paths.data.display());
            let backup = move_aside(&paths.data);
            eprintln!("Moved it to {}; starting over", backup.display());
            match open_data(&paths.data) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("Could not create {}: {e:#}. Changes will not be saved.", paths.data.display());
                    return Store::with_defaults();
                }
            }
        }
    };

    let cache = match attach_cache(&conn, &paths.cache) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cache {} is unreadable ({e:#}); starting it over", paths.cache.display());
            let _ = conn.execute_batch("DETACH DATABASE cache");
            remove_db_file(&paths.cache);
            match attach_cache(&conn, &paths.cache) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Could not create cache {} ({e:#}); keeping it in memory", paths.cache.display());
                    let _ = conn.execute_batch("DETACH DATABASE cache");
                    match attach_cache(&conn, Path::new(":memory:")) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("Could not set up storage ({e:#}). Changes will not be saved.");
                            return Store::with_defaults();
                        }
                    }
                }
            }
        }
    };

    let mut db = Db { conn, saved: Saved::default() };

    if fresh || data.is_empty() {
        // Nothing saved yet: bring over the old JSON store, or start with defaults.
        let (mut store, migrated) = match load_legacy_json(&paths.legacy_json) {
            Some(store) => (store, true),
            None => (Store::with_defaults(), false),
        };
        // A fresh data.db has no outbox, so a leftover cache can't be trusted
        // to line up with it; the first sync refills it.
        if let Err(e) = db.clear_cache() {
            eprintln!("Could not clear cache: {e:#}");
        }
        match db.save(&store) {
            Ok(()) if migrated => {
                let done = paths.legacy_json.with_extension("json.migrated");
                if let Err(e) = std::fs::rename(&paths.legacy_json, &done) {
                    eprintln!("Imported {} but could not rename it: {e}", paths.legacy_json.display());
                }
            }
            Ok(()) => {}
            Err(e) => eprintln!("Could not save: {e:#}"),
        }
        store.db = Some(db);
        return store;
    }

    db.assemble(data, cache)
}

/// What `data.db` holds.
struct DataPart {
    google: GoogleSettings,
    calendars: Vec<Calendar>,
    local: Vec<Event>,
    outbox: Vec<Event>,
    pending_deletes: Vec<PendingDelete>,
}

impl DataPart {
    /// Nothing has ever been saved (a new file, or one that was moved aside).
    fn is_empty(&self) -> bool {
        self.calendars.is_empty()
            && self.local.is_empty()
            && self.outbox.is_empty()
            && self.pending_deletes.is_empty()
            && self.google == GoogleSettings::default()
    }
}

/// What `cache.db` holds.
struct CachePart {
    events: Vec<Event>,
    sync_state: HashMap<String, SyncState>,
}

fn open_data(path: &Path) -> Result<(Connection, DataPart)> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let version = user_version(&conn, "main")?;
    if version > DATA_VERSION {
        return Err(TooNew(version).into());
    }
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS main.settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS main.calendars (
             id TEXT PRIMARY KEY,
             position INTEGER NOT NULL,
             name TEXT NOT NULL,
             color INTEGER NOT NULL,
             visible INTEGER NOT NULL,
             google_id TEXT,
             read_only INTEGER NOT NULL
         );
         {}
         {}
         CREATE TABLE IF NOT EXISTS main.pending_deletes (
             position INTEGER PRIMARY KEY,
             google_calendar_id TEXT NOT NULL,
             remote_id TEXT NOT NULL,
             etag TEXT
         );
         PRAGMA main.user_version = {DATA_VERSION};",
        event_table("main.events"),
        event_table("main.outbox"),
    ))?;
    let data = load_data(&conn).context("loading data")?;
    Ok((conn, data))
}

fn attach_cache(conn: &Connection, path: &Path) -> Result<CachePart> {
    conn.execute("ATTACH DATABASE ?1 AS cache", [path.to_string_lossy()])?;
    let version = user_version(conn, "cache")?;
    if version != 0 && version != CACHE_VERSION {
        // Different layout: it's only a cache, so start it over.
        conn.execute_batch("DROP TABLE IF EXISTS cache.events; DROP TABLE IF EXISTS cache.sync_state;")?;
    }
    conn.execute_batch(&format!(
        "{}
         CREATE TABLE IF NOT EXISTS cache.sync_state (
             google_calendar_id TEXT PRIMARY KEY,
             token TEXT,
             full_sync_on TEXT
         );
         PRAGMA cache.user_version = {CACHE_VERSION};",
        event_table("cache.events"),
    ))?;
    load_cache(conn).context("loading cache")
}

fn load_data(conn: &Connection) -> Result<DataPart> {
    let mut settings: HashMap<String, String> = conn
        .prepare("SELECT key, value FROM main.settings")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let google = GoogleSettings {
        client_id: settings.remove("google.client_id").unwrap_or_default(),
        client_secret: settings.remove("google.client_secret").unwrap_or_default(),
        refresh_token: settings.remove("google.refresh_token"),
        account_email: settings.remove("google.account_email"),
    };

    let calendars = conn
        .prepare("SELECT id, name, color, visible, google_id, read_only FROM main.calendars ORDER BY position")?
        .query_map([], |r| {
            let color: i64 = r.get(2)?;
            let google_id: Option<String> = r.get(4)?;
            Ok(Calendar {
                id: r.get(0)?,
                name: r.get(1)?,
                color: [(color >> 16) as u8, (color >> 8) as u8, color as u8],
                visible: r.get(3)?,
                source: match google_id {
                    Some(calendar_id) => Source::Google { calendar_id },
                    None => Source::Local,
                },
                read_only: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let pending_deletes = conn
        .prepare("SELECT google_calendar_id, remote_id, etag FROM main.pending_deletes ORDER BY position")?
        .query_map([], |r| Ok(PendingDelete { google_calendar_id: r.get(0)?, remote_id: r.get(1)?, etag: r.get(2)? }))?
        .collect::<rusqlite::Result<_>>()?;

    Ok(DataPart {
        google,
        calendars,
        local: load_events(conn, Place::Local)?,
        outbox: load_events(conn, Place::Outbox)?,
        pending_deletes,
    })
}

fn load_cache(conn: &Connection) -> Result<CachePart> {
    let sync_state = conn
        .prepare("SELECT google_calendar_id, token, full_sync_on FROM cache.sync_state")?
        .query_map([], |r| Ok((r.get(0)?, SyncState { token: r.get(1)?, full_sync_on: r.get(2)? })))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(CachePart { events: load_events(conn, Place::Cache)?, sync_state })
}

fn load_events(conn: &Connection, place: Place) -> rusqlite::Result<Vec<Event>> {
    conn.prepare(&format!("SELECT {EVENT_COLUMNS} FROM {} ORDER BY start_at, id", place.table()))?
        .query_map([], read_event)?
        .collect()
}

fn read_event(r: &Row) -> rusqlite::Result<Event> {
    let base: Option<String> = r.get(13)?;
    let base = match base {
        Some(text) => Some(serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(Event {
        id: r.get(0)?,
        calendar_id: r.get(1)?,
        title: r.get(2)?,
        start: r.get(3)?,
        end: r.get(4)?,
        all_day: r.get(5)?,
        location: r.get(6)?,
        notes: r.get(7)?,
        remote_id: r.get(8)?,
        recurring_id: r.get(9)?,
        pending: r.get(10)?,
        rev: r.get::<_, i64>(11)? as u64,
        etag: r.get(12)?,
        base,
    })
}

fn write_event(tx: &Transaction, place: Place, ev: &Event) -> Result<()> {
    let base = ev.base.as_ref().map(serde_json::to_string).transpose()?;
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {} ({EVENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        place.table()
    ))?
    .execute(params![
        ev.id,
        ev.calendar_id,
        ev.title,
        ev.start,
        ev.end,
        ev.all_day,
        ev.location,
        ev.notes,
        ev.remote_id,
        ev.recurring_id,
        ev.pending,
        ev.rev as i64,
        ev.etag,
        base,
    ])?;
    Ok(())
}

fn load_legacy_json(path: &Path) -> Option<Store> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("Could not read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(store) => Some(store),
        Err(e) => {
            eprintln!("Could not parse {}: {e}", path.display());
            let _ = std::fs::copy(path, path.with_extension("json.bak"));
            None
        }
    }
}

/// Renames an unreadable database out of the way and returns the new name.
fn move_aside(path: &Path) -> PathBuf {
    let backup = path.with_extension("db.bak");
    let _ = std::fs::remove_file(&backup);
    if let Err(e) = std::fs::rename(path, &backup) {
        eprintln!("Could not move {}: {e}", path.display());
    }
    let _ = std::fs::remove_file(journal(path));
    backup
}

fn remove_db_file(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(journal(path));
}

fn journal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-journal");
    PathBuf::from(name)
}

impl Db {
    /// Builds the in-memory store from what was loaded and remembers it as
    /// saved. An event in both the outbox and the cache is the outbox's
    /// (unconfirmed local edits win); the stale cached copy is dropped.
    fn assemble(mut self, data: DataPart, cache: CachePart) -> Store {
        let in_outbox: HashSet<String> = data.outbox.iter().map(|e| e.id.clone()).collect();
        let (stale, cached): (Vec<Event>, Vec<Event>) =
            cache.events.into_iter().partition(|e| in_outbox.contains(&e.id));
        for ev in &stale {
            if let Err(e) = self.conn.execute("DELETE FROM cache.events WHERE id = ?1", [&ev.id]) {
                eprintln!("Could not tidy cache: {e}");
            }
        }

        let mut events = Vec::new();
        for (place, list) in [(Place::Local, data.local), (Place::Cache, cached), (Place::Outbox, data.outbox)] {
            for ev in list {
                self.saved.events.insert(ev.id.clone(), (place, ev.clone()));
                events.push(ev);
            }
        }
        self.saved.calendars = data.calendars.clone();
        self.saved.google = data.google.clone();
        self.saved.pending_deletes = data.pending_deletes.clone();
        self.saved.sync_state = cache.sync_state.clone();

        Store {
            calendars: data.calendars,
            events,
            google: data.google,
            pending_deletes: data.pending_deletes,
            sync_state: cache.sync_state,
            db: Some(self),
        }
    }

    fn clear_cache(&mut self) -> Result<()> {
        self.conn.execute_batch("DELETE FROM cache.events; DELETE FROM cache.sync_state;")?;
        Ok(())
    }

    /// Writes whatever changed since the last save, in one transaction. If it
    /// fails nothing is written and the next save tries again.
    pub fn save(&mut self, s: &Store) -> Result<()> {
        let google_cals: HashSet<&str> =
            s.calendars.iter().filter(|c| c.google_id().is_some()).map(|c| c.id.as_str()).collect();
        let place = |e: &Event| {
            if !google_cals.contains(e.calendar_id.as_str()) {
                Place::Local
            } else if e.pending {
                Place::Outbox
            } else {
                Place::Cache
            }
        };

        let mut upserts: Vec<(Place, &Event)> = Vec::new();
        let mut deletes: Vec<(Place, String)> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for ev in &s.events {
            if !seen.insert(&ev.id) {
                bail!("two events share the id {}", ev.id);
            }
            let p = place(ev);
            match self.saved.events.get(&ev.id) {
                Some((sp, se)) if *sp == p && se == ev => {}
                Some((sp, _)) => {
                    if *sp != p {
                        deletes.push((*sp, ev.id.clone()));
                    }
                    upserts.push((p, ev));
                }
                None => upserts.push((p, ev)),
            }
        }
        for (id, (p, _)) in &self.saved.events {
            if !seen.contains(id.as_str()) {
                deletes.push((*p, id.clone()));
            }
        }
        let calendars_changed = s.calendars != self.saved.calendars;
        let google_changed = s.google != self.saved.google;
        let deletes_changed = s.pending_deletes != self.saved.pending_deletes;
        let sync_changed = s.sync_state != self.saved.sync_state;

        if upserts.is_empty() && deletes.is_empty() && !calendars_changed && !google_changed && !deletes_changed && !sync_changed
        {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        for (p, id) in &deletes {
            tx.prepare_cached(&format!("DELETE FROM {} WHERE id = ?1", p.table()))?.execute([id])?;
        }
        for (p, ev) in &upserts {
            write_event(&tx, *p, ev)?;
        }
        if google_changed {
            tx.execute("DELETE FROM main.settings WHERE key LIKE 'google.%'", [])?;
            let g = &s.google;
            let rows = [
                ("google.client_id", Some(&g.client_id)),
                ("google.client_secret", Some(&g.client_secret)),
                ("google.refresh_token", g.refresh_token.as_ref()),
                ("google.account_email", g.account_email.as_ref()),
            ];
            for (key, value) in rows {
                if let Some(value) = value {
                    tx.execute("INSERT INTO main.settings (key, value) VALUES (?1, ?2)", params![key, value])?;
                }
            }
        }
        if calendars_changed {
            tx.execute("DELETE FROM main.calendars", [])?;
            for (i, c) in s.calendars.iter().enumerate() {
                let color = (c.color[0] as i64) << 16 | (c.color[1] as i64) << 8 | c.color[2] as i64;
                tx.execute(
                    "INSERT INTO main.calendars (id, position, name, color, visible, google_id, read_only)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![c.id, i as i64, c.name, color, c.visible, c.google_id(), c.read_only],
                )?;
            }
        }
        if deletes_changed {
            tx.execute("DELETE FROM main.pending_deletes", [])?;
            for (i, d) in s.pending_deletes.iter().enumerate() {
                tx.execute(
                    "INSERT INTO main.pending_deletes (position, google_calendar_id, remote_id, etag)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![i as i64, d.google_calendar_id, d.remote_id, d.etag],
                )?;
            }
        }
        if sync_changed {
            tx.execute("DELETE FROM cache.sync_state", [])?;
            for (id, st) in &s.sync_state {
                tx.execute(
                    "INSERT INTO cache.sync_state (google_calendar_id, token, full_sync_on) VALUES (?1, ?2, ?3)",
                    params![id, st.token, st.full_sync_on],
                )?;
            }
        }
        tx.commit()?;

        for (_, id) in deletes {
            self.saved.events.remove(&id);
        }
        for (p, ev) in upserts {
            self.saved.events.insert(ev.id.clone(), (p, ev.clone()));
        }
        if calendars_changed {
            self.saved.calendars = s.calendars.clone();
        }
        if google_changed {
            self.saved.google = s.google.clone();
        }
        if deletes_changed {
            self.saved.pending_deletes = s.pending_deletes.clone();
        }
        if sync_changed {
            self.saved.sync_state = s.sync_state.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventFields;
    use chrono::{NaiveDate, NaiveDateTime};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("cleancalendar-db-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn paths(&self) -> Paths {
            Paths::in_dir(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const GCAL: &str = "g:me@example.com";

    fn at(h: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 14).unwrap().and_hms_opt(h, 0, 0).unwrap()
    }

    fn event(id: &str, calendar_id: &str, title: &str) -> Event {
        Event {
            id: id.into(),
            calendar_id: calendar_id.into(),
            title: title.into(),
            start: at(9),
            end: at(10),
            all_day: false,
            location: "Room 1".into(),
            notes: String::new(),
            remote_id: None,
            recurring_id: None,
            pending: false,
            rev: 0,
            etag: None,
            base: None,
        }
    }

    /// A store with one of everything.
    fn populated(dir: &TempDir) -> Store {
        let mut store = open(&dir.paths());
        store.google = GoogleSettings {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            refresh_token: Some("refresh".into()),
            account_email: Some("me@example.com".into()),
        };
        store.calendars.push(Calendar {
            id: GCAL.into(),
            name: "Me".into(),
            color: [1, 2, 3],
            visible: false,
            source: Source::Google { calendar_id: "me@example.com".into() },
            read_only: false,
        });
        store.events.push(event("local1", "local:home", "Dentist"));
        let mut synced = event("g1", GCAL, "Standup");
        synced.remote_id = Some("r1".into());
        synced.etag = Some("\"1\"".into());
        store.events.push(synced);
        let mut edited = event("g2", GCAL, "Edited offline");
        edited.remote_id = Some("r2".into());
        edited.pending = true;
        edited.rev = 3;
        edited.base = Some(EventFields { title: "Before".into(), ..edited.fields() });
        store.events.push(edited);
        store.pending_deletes.push(PendingDelete {
            google_calendar_id: "me@example.com".into(),
            remote_id: "r9".into(),
            etag: Some("\"9\"".into()),
        });
        store.sync_state.insert(
            "me@example.com".into(),
            SyncState { token: Some("tok".into()), full_sync_on: Some(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()) },
        );
        store.save();
        store
    }

    fn sorted(events: &[Event]) -> Vec<Event> {
        let mut v = events.to_vec();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    fn rows(path: &Path, table: &str) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn.prepare(&format!("SELECT id FROM {table} ORDER BY id")).unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn a_new_install_starts_with_the_default_calendars() {
        let dir = TempDir::new();
        let store = open(&dir.paths());
        assert_eq!(store.calendars.len(), 2);
        drop(store);
        let again = open(&dir.paths());
        assert_eq!(again.calendars, Store::with_defaults().calendars);
    }

    #[test]
    fn everything_round_trips() {
        let dir = TempDir::new();
        let store = populated(&dir);
        let (calendars, events, google, deletes, sync) = (
            store.calendars.clone(),
            store.events.clone(),
            store.google.clone(),
            store.pending_deletes.clone(),
            store.sync_state.clone(),
        );
        drop(store);

        let again = open(&dir.paths());
        assert_eq!(again.calendars, calendars);
        assert_eq!(sorted(&again.events), sorted(&events));
        assert_eq!(again.google, google);
        assert_eq!(again.pending_deletes, deletes);
        assert_eq!(again.sync_state, sync);
    }

    #[test]
    fn events_are_split_between_data_and_cache() {
        let dir = TempDir::new();
        let _store = populated(&dir);
        let p = dir.paths();
        assert_eq!(rows(&p.data, "events"), ["local1"]);
        assert_eq!(rows(&p.data, "outbox"), ["g2"]);
        assert_eq!(rows(&p.cache, "events"), ["g1"]);
    }

    #[test]
    fn a_confirmed_upload_moves_from_the_outbox_to_the_cache() {
        let dir = TempDir::new();
        let mut store = populated(&dir);
        let ev = store.event_mut("g2").unwrap();
        ev.pending = false;
        ev.base = None;
        store.save();
        let p = dir.paths();
        assert!(rows(&p.data, "outbox").is_empty());
        assert_eq!(rows(&p.cache, "events"), ["g1", "g2"]);

        // ...and back when edited again.
        store.event_mut("g2").unwrap().pending = true;
        store.save();
        assert_eq!(rows(&p.data, "outbox"), ["g2"]);
        assert_eq!(rows(&p.cache, "events"), ["g1"]);
    }

    #[test]
    fn deleted_events_are_removed() {
        let dir = TempDir::new();
        let mut store = populated(&dir);
        store.events.retain(|e| e.id != "local1" && e.id != "g1");
        store.save();
        drop(store);
        let again = open(&dir.paths());
        assert_eq!(again.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["g2"]);
    }

    #[test]
    fn edits_are_saved() {
        let dir = TempDir::new();
        let mut store = populated(&dir);
        store.event_mut("local1").unwrap().title = "Dentist (moved)".into();
        store.calendars[0].visible = false;
        store.save();
        drop(store);
        let again = open(&dir.paths());
        assert_eq!(again.event("local1").unwrap().title, "Dentist (moved)");
        assert!(!again.calendars[0].visible);
    }

    #[test]
    fn losing_the_cache_keeps_everything_that_is_not_on_google() {
        let dir = TempDir::new();
        let store = populated(&dir);
        drop(store);
        std::fs::remove_file(dir.paths().cache).unwrap();

        let again = open(&dir.paths());
        let mut ids: Vec<&str> = again.events.iter().map(|e| e.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["g2", "local1"], "the downloaded event is gone, local work is kept");
        assert!(again.sync_state.is_empty(), "no sync token, so the next sync is a full one");
        assert_eq!(again.google.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(again.calendars.len(), 3);
        assert_eq!(again.pending_deletes.len(), 1);
    }

    #[test]
    fn an_unreadable_cache_is_started_over() {
        let dir = TempDir::new();
        drop(populated(&dir));
        std::fs::write(dir.paths().cache, b"this is not a database, just junk bytes......").unwrap();

        let mut again = open(&dir.paths());
        assert!(again.event("g1").is_none());
        assert!(again.event("local1").is_some());
        again.sync_state.insert("me@example.com".into(), SyncState::default());
        again.save();
        assert!(rows(&dir.paths().cache, "events").is_empty());
    }

    #[test]
    fn an_unreadable_data_file_is_moved_aside() {
        let dir = TempDir::new();
        std::fs::write(dir.paths().data, b"this is not a database, just junk bytes......").unwrap();
        let store = open(&dir.paths());
        assert_eq!(store.calendars, Store::with_defaults().calendars);
        assert!(dir.0.join("data.db.bak").exists());
    }

    #[test]
    fn an_old_json_store_is_imported_once() {
        let dir = TempDir::new();
        let json = r#"{
            "calendars": [
                {"id": "local:home", "name": "Home", "color": [0, 122, 255], "visible": true, "source": "Local"},
                {"id": "g:me@example.com", "name": "Me", "color": [1, 2, 3], "visible": true,
                 "source": {"Google": {"calendar_id": "me@example.com"}}}
            ],
            "events": [
                {"id": "a", "calendar_id": "local:home", "title": "Lunch",
                 "start": "2026-11-14T12:00:00", "end": "2026-11-14T13:00:00", "all_day": false},
                {"id": "b", "calendar_id": "g:me@example.com", "title": "Sync me",
                 "start": "2026-11-14T15:00:00", "end": "2026-11-14T16:00:00", "all_day": false,
                 "remote_id": "rb", "etag": "\"2\""}
            ],
            "google": {"client_id": "cid", "client_secret": "s", "refresh_token": "rt", "account_email": null},
            "sync_state": {"me@example.com": {"token": "t", "full_sync_on": "2026-11-01"}}
        }"#;
        std::fs::write(dir.paths().legacy_json, json).unwrap();

        let store = open(&dir.paths());
        assert_eq!(store.events.len(), 2);
        assert_eq!(store.google.refresh_token.as_deref(), Some("rt"));
        assert!(!dir.paths().legacy_json.exists());
        assert!(dir.0.join("store.json.migrated").exists());
        drop(store);

        let again = open(&dir.paths());
        assert_eq!(again.calendars.len(), 2);
        assert_eq!(again.event("b").unwrap().etag.as_deref(), Some("\"2\""));
        assert_eq!(again.sync_state["me@example.com"].token.as_deref(), Some("t"));
        assert_eq!(rows(&dir.paths().cache, "events"), ["b"]);
    }

    /// Imports a real `store.json` (a copy is made; the original is not
    /// touched) and checks that reopening gives back exactly what it held:
    /// `CLEANCALENDAR_IMPORT_CHECK=/path/to/store.json cargo test import_check -- --ignored`
    #[test]
    #[ignore]
    fn import_check() {
        let src = std::env::var_os("CLEANCALENDAR_IMPORT_CHECK").expect("set CLEANCALENDAR_IMPORT_CHECK");
        let expected: Store = serde_json::from_str(&std::fs::read_to_string(&src).unwrap()).unwrap();
        let dir = TempDir::new();
        std::fs::copy(&src, dir.paths().legacy_json).unwrap();
        drop(open(&dir.paths()));
        let got = open(&dir.paths());

        assert_eq!(got.calendars, expected.calendars);
        assert_eq!(sorted(&got.events), sorted(&expected.events));
        assert_eq!(got.google, expected.google);
        assert_eq!(got.pending_deletes, expected.pending_deletes);
        assert_eq!(got.sync_state, expected.sync_state);
        let p = dir.paths();
        let (local, outbox, cache) = (rows(&p.data, "events"), rows(&p.data, "outbox"), rows(&p.cache, "events"));
        println!(
            "{} calendars, {} events: {} local, {} unsynced, {} cached; {} pending deletes, {} sync tokens",
            got.calendars.len(),
            got.events.len(),
            local.len(),
            outbox.len(),
            cache.len(),
            got.pending_deletes.len(),
            got.sync_state.len(),
        );
        assert_eq!(local.len() + outbox.len() + cache.len(), got.events.len());
    }

    #[test]
    fn a_failed_save_is_retried_by_the_next_one() {
        let dir = TempDir::new();
        let mut store = populated(&dir);
        store.events.push(event("dup", "local:home", "One"));
        store.events.push(event("dup", "local:home", "Two"));
        store.save(); // refused: duplicate id
        store.events.pop();
        store.save();
        drop(store);
        assert!(open(&dir.paths()).event("dup").is_some());
    }
}
