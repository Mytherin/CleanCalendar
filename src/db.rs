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
use crate::settings::Settings;

/// The `main.settings` row holding the app's preferences (as JSON).
const PREFS_KEY: &str = "app.settings";

const DATA_VERSION: i32 = 2;
/// Bumped when cached events gain fields, so they are downloaded again.
const CACHE_VERSION: i32 = 2;

/// Where the two databases (and the old JSON store, for migration) live, and
/// where the Google refresh token is kept.
#[derive(Clone, Debug)]
pub struct Paths {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub legacy_json: PathBuf,
    pub secrets: Secrets,
}

/// The app's folder name inside the OS's data and cache folders. Linux
/// convention is lowercase.
const APP_DIR: &str = if cfg!(any(windows, target_os = "macos")) { "CleanCalendar" } else { "cleancalendar" };

impl Paths {
    /// `CLEANCALENDAR_DATA=<dir>` puts everything in one folder; otherwise the
    /// OS's per-user data and cache folders are used:
    ///
    /// | | data.db | cache.db |
    /// |---|---|---|
    /// | Windows | `%APPDATA%\CleanCalendar` | `%LOCALAPPDATA%\CleanCalendar` |
    /// | macOS | `~/Library/Application Support/CleanCalendar` | `~/Library/Caches/CleanCalendar` |
    /// | Linux | `~/.local/share/cleancalendar` | `~/.cache/cleancalendar` |
    ///
    /// Files left in the folders older versions used are moved over.
    pub fn default_location() -> Self {
        if let Some(dir) = std::env::var_os("CLEANCALENDAR_DATA") {
            let dir = PathBuf::from(dir);
            let account = format!("{REFRESH_TOKEN}@{}", std::path::absolute(&dir).unwrap_or(dir.clone()).display());
            return Paths::in_dir(&dir, Secrets::Keyring(account));
        }
        let Some(base) = directories::BaseDirs::new() else {
            return Paths::in_dir(Path::new("."), Secrets::Keyring(REFRESH_TOKEN.into()));
        };
        let paths = Paths {
            data: base.data_dir().join(APP_DIR).join("data.db"),
            cache: base.cache_dir().join(APP_DIR).join("cache.db"),
            legacy_json: base.data_dir().join(APP_DIR).join("store.json"),
            secrets: Secrets::Keyring(REFRESH_TOKEN.into()),
        };
        if let Some(old) = directories::ProjectDirs::from("com", "CleanCalendar", "CleanCalendar") {
            move_from_old_folders(old.data_dir(), old.cache_dir(), &paths);
        }
        paths
    }

    pub fn in_dir(dir: &Path, secrets: Secrets) -> Self {
        Paths { data: dir.join("data.db"), cache: dir.join("cache.db"), legacy_json: dir.join("store.json"), secrets }
    }
}

/// Moves the files from the folders versions before 2026-11 used
/// (`directories::ProjectDirs`, e.g. `%APPDATA%\CleanCalendar\CleanCalendar\data`)
/// to the current ones. Does nothing once the new folder has data, or where
/// the two are the same folder (Linux). If `data.db` can't be moved the old
/// files are left alone, and the app starts with an empty new folder rather
/// than splitting the database from its journal.
fn move_from_old_folders(old_data: &Path, old_cache: &Path, new: &Paths) {
    let new_data = new.data.parent().unwrap_or(Path::new("."));
    let new_cache = new.cache.parent().unwrap_or(Path::new("."));
    if old_data == new_data || new.data.exists() || new.legacy_json.exists() {
        return;
    }
    let data_files = ["data.db", "data.db-wal", "data.db-shm", "data.db-journal", "store.json", "store.json.migrated"];
    if !data_files.iter().any(|f| old_data.join(f).exists()) {
        return;
    }
    if let Err(e) = move_files(old_data, new_data, &data_files) {
        eprintln!("Could not move data from {} to {}: {e}", old_data.display(), new_data.display());
        return;
    }
    // The cache can always be downloaded again, so a failure here is fine.
    if old_cache != new_cache {
        if let Err(e) = move_files(old_cache, new_cache, &["cache.db", "cache.db-wal", "cache.db-shm", "cache.db-journal"]) {
            eprintln!("Could not move cache from {}: {e}; it will be downloaded again", old_cache.display());
        }
    }
    // Tidy up the old folders if nothing else is in them (on Windows this is
    // `...\CleanCalendar\CleanCalendar\data` and its parent).
    for dir in [old_data, old_cache] {
        let _ = std::fs::remove_dir(dir);
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

fn move_files(from: &Path, to: &Path, names: &[&str]) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for name in names {
        let src = from.join(name);
        if src.exists() {
            std::fs::rename(&src, to.join(name))?;
        }
    }
    Ok(())
}

/// The credential-store entry the refresh token is kept under.
const KEYRING_SERVICE: &str = "CleanCalendar";
const REFRESH_TOKEN: &str = "google-refresh-token";

/// Where the Google refresh token is kept: the OS credential store (Windows
/// Credential Manager, macOS Keychain, Secret Service on Linux) under the
/// given account name. Tests use an in-memory one so they never touch the
/// real store.
#[derive(Clone, Debug)]
pub enum Secrets {
    Keyring(String),
    #[cfg(test)]
    Memory(std::sync::Arc<std::sync::Mutex<Option<String>>>),
    /// A credential store that always fails, to test the fallback.
    #[cfg(test)]
    Unavailable,
}

impl Secrets {
    fn entry(account: &str) -> keyring::Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, account)
    }

    fn get(&self) -> Result<Option<String>> {
        match self {
            Secrets::Keyring(account) => match Self::entry(account)?.get_password() {
                Ok(token) => Ok(Some(token)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.into()),
            },
            #[cfg(test)]
            Secrets::Memory(m) => Ok(m.lock().unwrap().clone()),
            #[cfg(test)]
            Secrets::Unavailable => bail!("no credential store"),
        }
    }

    fn set(&self, token: Option<&str>) -> Result<()> {
        match self {
            Secrets::Keyring(account) => {
                let entry = Self::entry(account)?;
                match token {
                    Some(t) => entry.set_password(t)?,
                    None => match entry.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(e) => return Err(e.into()),
                    },
                }
                Ok(())
            }
            #[cfg(test)]
            Secrets::Memory(m) => {
                *m.lock().unwrap() = token.map(str::to_owned);
                Ok(())
            }
            #[cfg(test)]
            Secrets::Unavailable => bail!("no credential store"),
        }
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
    settings: Settings,
    events: HashMap<String, (Place, Event)>,
}

#[derive(Debug)]
pub struct Db {
    conn: Connection,
    saved: Saved,
    secrets: Secrets,
    /// The refresh token is in `main.settings` because the credential store
    /// couldn't be used (or it was saved by an older version).
    token_in_db: bool,
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
                             remote_id, recurring_id, pending, rev, etag, base, recurrence, time_zone";

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
            base TEXT,
            recurrence TEXT,
            time_zone TEXT
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

    let mut db = Db { conn, saved: Saved::default(), secrets: paths.secrets.clone(), token_in_db: false };
    let mut data = data;
    db.load_token(&mut data.google);

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
    settings: Settings,
}

impl DataPart {
    /// Nothing has ever been saved (a new file, or one that was moved aside).
    fn is_empty(&self) -> bool {
        self.calendars.is_empty()
            && self.local.is_empty()
            && self.outbox.is_empty()
            && self.pending_deletes.is_empty()
            && self.google == GoogleSettings::default()
            && self.settings == Settings::default()
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
    if version == 1 {
        // Version 2 added repeating events and time zones.
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE main.events ADD COLUMN recurrence TEXT;
             ALTER TABLE main.events ADD COLUMN time_zone TEXT;
             ALTER TABLE main.outbox ADD COLUMN recurrence TEXT;
             ALTER TABLE main.outbox ADD COLUMN time_zone TEXT;
             PRAGMA main.user_version = 2;
             COMMIT;",
        )?;
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
    // Unreadable preferences (e.g. hand-edited) fall back to the defaults
    // rather than failing the whole load.
    let prefs = match settings.remove(PREFS_KEY) {
        Some(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            eprintln!("Ignoring unreadable settings ({e}); using defaults");
            Settings::default()
        }),
        None => Settings::default(),
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
        settings: prefs,
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

/// A column holding JSON (or NULL).
fn json_column<T: serde::de::DeserializeOwned>(r: &Row, i: usize) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = r.get(i)?;
    text.map(|text| {
        serde_json::from_str(&text)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e)))
    })
    .transpose()
}

fn read_event(r: &Row) -> rusqlite::Result<Event> {
    let base = json_column(r, 13)?;
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
        recurrence: json_column(r, 14)?,
        time_zone: r.get(15)?,
    })
}

fn write_event(tx: &Transaction, place: Place, ev: &Event) -> Result<()> {
    let base = ev.base.as_ref().map(serde_json::to_string).transpose()?;
    let recurrence = ev.recurrence.as_ref().map(serde_json::to_string).transpose()?;
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {} ({EVENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
        recurrence,
        ev.time_zone,
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
    /// Fills in the refresh token. A token found in `main.settings` (saved by
    /// an older version, or because the credential store failed last time) is
    /// the newest one; it is moved into the credential store when possible.
    fn load_token(&mut self, google: &mut GoogleSettings) {
        if let Some(token) = &google.refresh_token {
            match self.secrets.set(Some(token)) {
                Ok(()) => match self.conn.execute("DELETE FROM main.settings WHERE key = 'google.refresh_token'", []) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("Could not remove the refresh token from data.db: {e}");
                        self.token_in_db = true;
                    }
                },
                Err(e) => {
                    eprintln!("Credential store unavailable ({e:#}); the refresh token stays in data.db");
                    self.token_in_db = true;
                }
            }
            return;
        }
        match self.secrets.get() {
            Ok(token) => google.refresh_token = token,
            Err(e) => eprintln!("Could not read the refresh token from the credential store: {e:#}"),
        }
    }

    /// Writes the refresh token to the credential store. Returns whether it
    /// must go in `main.settings` instead, because that failed.
    fn store_token(&mut self, token: Option<&String>) -> bool {
        let changed = token != self.saved.google.refresh_token.as_ref();
        if changed || self.token_in_db {
            match self.secrets.set(token.map(String::as_str)) {
                Ok(()) => self.token_in_db = false,
                Err(e) => {
                    eprintln!("Credential store unavailable ({e:#}); keeping the refresh token in data.db");
                    self.token_in_db = true;
                }
            }
        }
        self.token_in_db
    }

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
        self.saved.settings = data.settings.clone();

        Store {
            calendars: data.calendars,
            events,
            google: data.google,
            pending_deletes: data.pending_deletes,
            sync_state: cache.sync_state,
            settings: data.settings,
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
        let settings_changed = s.settings != self.saved.settings;

        if upserts.is_empty()
            && deletes.is_empty()
            && !calendars_changed
            && !google_changed
            && !deletes_changed
            && !sync_changed
            && !settings_changed
        {
            return Ok(());
        }

        let token_in_db = google_changed && self.store_token(s.google.refresh_token.as_ref());

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
                ("google.refresh_token", g.refresh_token.as_ref().filter(|_| token_in_db)),
                ("google.account_email", g.account_email.as_ref()),
            ];
            for (key, value) in rows {
                if let Some(value) = value {
                    tx.execute("INSERT INTO main.settings (key, value) VALUES (?1, ?2)", params![key, value])?;
                }
            }
        }
        if settings_changed {
            tx.execute(
                "INSERT OR REPLACE INTO main.settings (key, value) VALUES (?1, ?2)",
                params![PREFS_KEY, serde_json::to_string(&s.settings)?],
            )?;
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
        if settings_changed {
            self.saved.settings = s.settings.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventFields;
    use chrono::{NaiveDate, NaiveDateTime};

    /// A scratch folder, plus an in-memory credential store that lasts as
    /// long as it does.
    struct TempDir(PathBuf, Secrets);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("cleancalendar-db-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir, Secrets::Memory(Default::default()))
        }
        fn paths(&self) -> Paths {
            Paths::in_dir(&self.0, self.1.clone())
        }
        fn token(&self) -> Option<String> {
            self.1.get().unwrap()
        }
    }

    fn setting_keys(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn.prepare("SELECT key FROM settings ORDER BY key").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
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
            recurrence: None,
            time_zone: None,
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
        store.settings.days_per_week = 5;
        store.settings.week_start = Some(6);
        store.settings.google_description = "Work".into();
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
        let (calendars, events, google, deletes, sync, settings) = (
            store.calendars.clone(),
            store.events.clone(),
            store.google.clone(),
            store.pending_deletes.clone(),
            store.sync_state.clone(),
            store.settings.clone(),
        );
        drop(store);

        let again = open(&dir.paths());
        assert_eq!(again.calendars, calendars);
        assert_eq!(sorted(&again.events), sorted(&events));
        assert_eq!(again.google, google);
        assert_eq!(again.pending_deletes, deletes);
        assert_eq!(again.sync_state, sync);
        assert_eq!(again.settings, settings);
        assert_eq!(again.settings.days_per_week, 5);
    }

    #[test]
    fn signing_out_keeps_the_settings() {
        let dir = TempDir::new();
        let mut store = populated(&dir);
        store.google = GoogleSettings::default();
        store.save();
        drop(store);
        assert_eq!(open(&dir.paths()).settings.google_description, "Work");
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
    fn the_refresh_token_is_kept_in_the_credential_store() {
        let dir = TempDir::new();
        drop(populated(&dir));
        assert_eq!(dir.token().as_deref(), Some("refresh"));
        assert_eq!(
            setting_keys(&dir.paths().data),
            ["app.settings", "google.account_email", "google.client_id", "google.client_secret"],
            "no token in data.db"
        );

        let mut again = open(&dir.paths());
        assert_eq!(again.google.refresh_token.as_deref(), Some("refresh"));
        again.google.refresh_token = None; // signing out
        again.save();
        assert_eq!(dir.token(), None);
        drop(again);
        assert_eq!(open(&dir.paths()).google.refresh_token, None);
    }

    /// Round-trips a throwaway entry through the real OS credential store:
    /// `cargo test real_credential_store -- --ignored`
    #[test]
    #[ignore]
    fn real_credential_store() {
        let s = Secrets::Keyring(format!("test-{}", uuid::Uuid::new_v4().simple()));
        assert_eq!(s.get().unwrap(), None);
        s.set(Some("secret")).unwrap();
        assert_eq!(s.get().unwrap().as_deref(), Some("secret"));
        s.set(None).unwrap();
        assert_eq!(s.get().unwrap(), None);
        s.set(None).unwrap(); // deleting twice is fine
    }

    #[test]
    fn without_a_credential_store_the_token_stays_in_data_db() {
        let dir = TempDir::new();
        let broken = Paths { secrets: Secrets::Unavailable, ..dir.paths() };
        let mut store = open(&broken);
        store.google.client_id = "cid".into();
        store.google.refresh_token = Some("rt".into());
        store.save();
        drop(store);
        assert!(setting_keys(&dir.paths().data).contains(&"google.refresh_token".to_string()));
        assert_eq!(open(&broken).google.refresh_token.as_deref(), Some("rt"));

        // Once a credential store works, the token is moved into it.
        let store = open(&dir.paths());
        assert_eq!(store.google.refresh_token.as_deref(), Some("rt"));
        assert_eq!(dir.token().as_deref(), Some("rt"));
        assert!(!setting_keys(&dir.paths().data).contains(&"google.refresh_token".to_string()));
        drop(store);
        assert_eq!(open(&dir.paths()).google.refresh_token.as_deref(), Some("rt"));
    }

    #[test]
    fn files_in_the_old_folders_are_moved() {
        let root = TempDir::new();
        let old_data = root.0.join("Old").join("App").join("data");
        let old_cache = root.0.join("OldLocal").join("App").join("cache");
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::create_dir_all(&old_cache).unwrap();
        std::fs::write(old_data.join("data.db"), b"data").unwrap();
        std::fs::write(old_data.join("store.json.migrated"), b"json").unwrap();
        std::fs::write(old_cache.join("cache.db"), b"cache").unwrap();
        let new = Paths {
            data: root.0.join("New").join("data.db"),
            cache: root.0.join("NewLocal").join("cache.db"),
            legacy_json: root.0.join("New").join("store.json"),
            secrets: root.1.clone(),
        };

        move_from_old_folders(&old_data, &old_cache, &new);
        assert_eq!(std::fs::read(&new.data).unwrap(), b"data");
        assert_eq!(std::fs::read(&new.cache).unwrap(), b"cache");
        assert!(root.0.join("New").join("store.json.migrated").exists());
        assert!(!root.0.join("Old").join("App").exists(), "empty old folders are removed");
        assert!(!root.0.join("OldLocal").join("App").exists());

        // Once the new folder has data, the old one is left alone.
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::write(old_data.join("data.db"), b"older").unwrap();
        move_from_old_folders(&old_data, &old_cache, &new);
        assert_eq!(std::fs::read(&new.data).unwrap(), b"data");
        assert!(old_data.join("data.db").exists());
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
        assert_eq!(dir.token().as_deref(), Some("rt"), "the imported token goes to the credential store");
        assert!(!setting_keys(&dir.paths().data).contains(&"google.refresh_token".to_string()));
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

    /// Opens a copy of a real `data.db` from an older version and checks
    /// nothing is lost in the upgrade:
    /// `CLEANCALENDAR_UPGRADE_CHECK=/path/to/data.db cargo test upgrade_check -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn upgrade_check() {
        let src = std::env::var_os("CLEANCALENDAR_UPGRADE_CHECK").expect("set CLEANCALENDAR_UPGRADE_CHECK");
        let dir = TempDir::new();
        let p = dir.paths();
        std::fs::copy(&src, &p.data).unwrap();
        let before = (rows(&p.data, "events"), rows(&p.data, "outbox"));
        let old_version = user_version(&Connection::open(&p.data).unwrap(), "main").unwrap();

        let store = open(&p);
        assert_eq!((rows(&p.data, "events"), rows(&p.data, "outbox")), before);
        let mut ids: Vec<String> = store.events.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        let mut expected = [before.0.clone(), before.1.clone()].concat();
        expected.sort();
        assert_eq!(ids, expected);
        assert_eq!(user_version(&Connection::open(&p.data).unwrap(), "main").unwrap(), DATA_VERSION);
        println!(
            "schema {old_version} -> {DATA_VERSION}: {} calendars, {} local events, {} unsynced kept",
            store.calendars.len(),
            before.0.len(),
            before.1.len()
        );
    }

    #[test]
    fn repeat_rules_and_time_zones_are_saved() {
        use crate::recurrence::{Freq, Recurrence, RepeatEnd};
        let dir = TempDir::new();
        let mut store = populated(&dir);
        let mut rule = Recurrence::new(Freq::Weekly);
        rule.weekdays = vec![chrono::Weekday::Mon, chrono::Weekday::Fri];
        rule.end = RepeatEnd::Until(NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
        rule.exceptions = vec![at(9) + chrono::Duration::days(3)];
        let ev = store.event_mut("local1").unwrap();
        ev.recurrence = Some(rule);
        ev.time_zone = Some("America/New_York".into());
        // A pending edit's base snapshot carries them too.
        let g2 = store.event_mut("g2").unwrap();
        g2.time_zone = Some("Asia/Tokyo".into());
        g2.base.as_mut().unwrap().time_zone = Some("Europe/Paris".into());
        store.save();
        let events = store.events.clone();
        drop(store);
        assert_eq!(sorted(&open(&dir.paths()).events), sorted(&events));
    }

    #[test]
    fn a_version_1_data_file_is_upgraded_in_place() {
        let dir = TempDir::new();
        let p = dir.paths();
        {
            // The event tables as version 1 had them.
            let conn = Connection::open(&p.data).unwrap();
            let v1 = |name: &str| {
                format!(
                    "CREATE TABLE {name} (id TEXT PRIMARY KEY, calendar_id TEXT NOT NULL, title TEXT NOT NULL,
                     start_at TEXT NOT NULL, end_at TEXT NOT NULL, all_day INTEGER NOT NULL, location TEXT NOT NULL,
                     notes TEXT NOT NULL, remote_id TEXT, recurring_id TEXT, pending INTEGER NOT NULL,
                     rev INTEGER NOT NULL, etag TEXT, base TEXT);"
                )
            };
            conn.execute_batch(&format!("{} {} PRAGMA user_version = 1;", v1("events"), v1("outbox"))).unwrap();
            conn.execute(
                "INSERT INTO events VALUES ('local1', 'local:home', 'Dentist', ?1, ?2, 0, '', '', NULL, NULL, 0, 0, NULL, ?3)",
                params![at(9), at(10), r#"{"title":"Dentist","location":"","notes":"","start":"2026-11-14T09:00:00","end":"2026-11-14T10:00:00","all_day":false}"#],
            )
            .unwrap();
        }
        let mut store = open(&p);
        let ev = store.event("local1").expect("kept");
        assert_eq!((ev.title.as_str(), &ev.recurrence, &ev.time_zone), ("Dentist", &None, &None));
        assert_eq!(ev.base.as_ref().unwrap().time_zone, None, "old snapshots read without the new fields");

        store.event_mut("local1").unwrap().time_zone = Some("Europe/Amsterdam".into());
        store.save();
        drop(store);
        let again = open(&p);
        assert_eq!(again.event("local1").unwrap().time_zone.as_deref(), Some("Europe/Amsterdam"));
        assert_eq!(user_version(&Connection::open(&p.data).unwrap(), "main").unwrap(), DATA_VERSION);
    }
}
