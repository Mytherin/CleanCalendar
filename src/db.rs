//! On-disk storage, split by who owns the data:
//!
//! - `data.db` (persistent, in the user's data folder): account settings
//!   (Google sign-in, CalDAV accounts), the calendar list with the user's
//!   colours and visibility, events on local calendars, and local changes to
//!   synced events that haven't been uploaded yet (`outbox`,
//!   `pending_deletes`). None of this can be rebuilt from a server, so it is
//!   never thrown away. Secrets (the Google refresh token, CalDAV passwords)
//!   go to the OS credential store instead when it works.
//! - `cache.db` (disposable, in the user's cache folder): synced events as
//!   last downloaded, and the incremental-sync tokens. The server is the
//!   source of truth here; if the file is deleted or unreadable it is started
//!   over and the next sync downloads everything again.
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

use crate::model::{CalDavAccount, Calendar, Event, GoogleSettings, PendingDelete, Source, Store, SyncState};
use crate::remote::{CalRef, GOOGLE};
use crate::settings::Settings;

/// The `main.settings` row holding the app's preferences (as JSON).
const PREFS_KEY: &str = "app.settings";

/// 2 added repeating events and time zones, 3 invitees, 4 CalDAV accounts.
const DATA_VERSION: i32 = 4;
/// Bumped when cached events gain fields (or sync state changes meaning),
/// so they are downloaded again.
const CACHE_VERSION: i32 = 5;

/// Where the two databases (and the old JSON store, for migration) live, and
/// where secrets are kept.
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
            let suffix = format!("@{}", std::path::absolute(&dir).unwrap_or(dir.clone()).display());
            return Paths::in_dir(&dir, Secrets::Keyring(suffix));
        }
        let Some(base) = directories::BaseDirs::new() else {
            return Paths::in_dir(Path::new("."), Secrets::Keyring(String::new()));
        };
        let paths = Paths {
            data: base.data_dir().join(APP_DIR).join("data.db"),
            cache: base.cache_dir().join(APP_DIR).join("cache.db"),
            legacy_json: base.data_dir().join(APP_DIR).join("store.json"),
            secrets: Secrets::Keyring(String::new()),
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

/// The credential-store service secrets are kept under.
const KEYRING_SERVICE: &str = "CleanCalendar";
/// The key of the Google refresh token.
const REFRESH_TOKEN: &str = "google-refresh-token";

/// The key of a CalDAV account's password.
fn password_key(account: &str) -> String {
    format!("{account}-password")
}

/// Where secrets are kept: the OS credential store (Windows Credential
/// Manager, macOS Keychain, Secret Service on Linux), one entry per key, its
/// name the key plus the given suffix (to keep separate data folders apart).
/// Tests use an in-memory one so they never touch the real store.
#[derive(Clone, Debug)]
pub enum Secrets {
    Keyring(String),
    #[cfg(test)]
    Memory(std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>),
    /// A credential store that always fails, to test the fallback.
    #[cfg(test)]
    Unavailable,
}

impl Secrets {
    fn entry(name: &str) -> keyring::Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, name)
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        match self {
            Secrets::Keyring(suffix) => match Self::entry(&format!("{key}{suffix}"))?.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.into()),
            },
            #[cfg(test)]
            Secrets::Memory(m) => Ok(m.lock().unwrap().get(key).cloned()),
            #[cfg(test)]
            Secrets::Unavailable => bail!("no credential store"),
        }
    }

    fn set(&self, key: &str, secret: Option<&str>) -> Result<()> {
        match self {
            Secrets::Keyring(suffix) => {
                let entry = Self::entry(&format!("{key}{suffix}"))?;
                match secret {
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
                let mut m = m.lock().unwrap();
                match secret {
                    Some(s) => m.insert(key.to_owned(), s.to_owned()),
                    None => m.remove(key),
                };
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
    /// On a synced calendar with unconfirmed local changes: `main.outbox`.
    Outbox,
    /// On a synced calendar, as the server has it: `cache.events`.
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
    caldav: Vec<CalDavAccount>,
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
    /// Keys of secrets kept in `data.db` because the credential store
    /// couldn't be used (or that an older version saved there).
    in_db: HashSet<String>,
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
                             remote_id, recurring_id, pending, rev, etag, base, recurrence, time_zone, attendees";

/// Columns added to `events` and `outbox` since version 1, so older files
/// can be brought up to date (whichever of them they already have).
const ADDED_EVENT_COLUMNS: [(&str, &str); 3] =
    [("recurrence", "TEXT"), ("time_zone", "TEXT"), ("attendees", "TEXT NOT NULL DEFAULT '[]'")];

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
            time_zone TEXT,
            attendees TEXT NOT NULL DEFAULT '[]'
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

    let mut db = Db { conn, saved: Saved::default(), secrets: paths.secrets.clone(), in_db: HashSet::new() };
    let mut data = data;
    db.load_secrets(&mut data);

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
    /// With the password if it was kept in `data.db`.
    caldav: Vec<CalDavAccount>,
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
            && self.caldav.is_empty()
            && self.google == GoogleSettings::default()
            && self.settings == Settings::default()
    }
}

/// Whether `table` exists and has `column`.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA main.table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|n| n == column))
}

/// Version 4 added CalDAV accounts: calendars and pending deletes say which
/// account they belong to, and their Google-specific columns got neutral
/// names. Everything already there is Google's. Safe to run on a file that
/// already has these changes.
fn upgrade_to_accounts(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if has_column(&tx, "calendars", "google_id")? {
        tx.execute_batch(
            "ALTER TABLE main.calendars RENAME COLUMN google_id TO remote_id;
             ALTER TABLE main.calendars ADD COLUMN account TEXT;
             UPDATE main.calendars SET account = 'google' WHERE remote_id IS NOT NULL;",
        )?;
    }
    if has_column(&tx, "pending_deletes", "google_calendar_id")? {
        tx.execute_batch(
            "ALTER TABLE main.pending_deletes RENAME COLUMN google_calendar_id TO calendar_id;
             ALTER TABLE main.pending_deletes ADD COLUMN account TEXT NOT NULL DEFAULT 'google';",
        )?;
    }
    tx.commit()?;
    Ok(())
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
    if (1..DATA_VERSION).contains(&version) {
        // Add the event columns this file doesn't have yet. Checking each one
        // (rather than going by the version) also covers files written by
        // builds that added them in a different order.
        let mut sql = String::from("BEGIN;");
        for table in ["events", "outbox"] {
            let have: HashSet<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}', 'main')"))?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for (col, decl) in ADDED_EVENT_COLUMNS.iter().filter(|(col, _)| !have.contains(*col)) {
                sql += &format!("ALTER TABLE main.{table} ADD COLUMN {col} {decl};");
            }
        }
        sql += "COMMIT;";
        conn.execute_batch(&sql)?;
        upgrade_to_accounts(&conn)?;
        // The version is raised below, once every table is in place; if the
        // app stops before then, these steps simply run again.
    }
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS main.settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS main.calendars (
             id TEXT PRIMARY KEY,
             position INTEGER NOT NULL,
             name TEXT NOT NULL,
             color INTEGER NOT NULL,
             visible INTEGER NOT NULL,
             remote_id TEXT,
             read_only INTEGER NOT NULL,
             account TEXT
         );
         {}
         {}
         CREATE TABLE IF NOT EXISTS main.pending_deletes (
             position INTEGER PRIMARY KEY,
             calendar_id TEXT NOT NULL,
             remote_id TEXT NOT NULL,
             etag TEXT,
             account TEXT NOT NULL DEFAULT 'google'
         );
         CREATE TABLE IF NOT EXISTS main.accounts (
             id TEXT PRIMARY KEY,
             position INTEGER NOT NULL,
             label TEXT NOT NULL,
             server TEXT NOT NULL,
             username TEXT NOT NULL,
             -- Only when the credential store can't be used.
             password TEXT
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
             calendar_id TEXT PRIMARY KEY,
             token TEXT,
             full_sync_on TEXT,
             window_from TEXT
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
        .prepare("SELECT id, name, color, visible, remote_id, read_only, account FROM main.calendars ORDER BY position")?
        .query_map([], |r| {
            let color: i64 = r.get(2)?;
            let remote_id: Option<String> = r.get(4)?;
            let account: Option<String> = r.get(6)?;
            Ok(Calendar {
                id: r.get(0)?,
                name: r.get(1)?,
                color: [(color >> 16) as u8, (color >> 8) as u8, color as u8],
                visible: r.get(3)?,
                source: match (remote_id, account) {
                    (None, _) => Source::Local,
                    (Some(calendar_id), None) => Source::Google { calendar_id },
                    (Some(calendar_id), Some(a)) if a == GOOGLE => Source::Google { calendar_id },
                    (Some(url), Some(account)) => Source::CalDav { account, url },
                },
                read_only: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let pending_deletes = conn
        .prepare("SELECT account, calendar_id, remote_id, etag FROM main.pending_deletes ORDER BY position")?
        .query_map([], |r| {
            let (account, calendar): (String, String) = (r.get(0)?, r.get(1)?);
            Ok(PendingDelete { calendar: CalRef::new(&account, &calendar), remote_id: r.get(2)?, etag: r.get(3)? })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let caldav = conn
        .prepare("SELECT id, label, server, username, password FROM main.accounts ORDER BY position")?
        .query_map([], |r| {
            Ok(CalDavAccount {
                id: r.get(0)?,
                label: r.get(1)?,
                server: r.get(2)?,
                username: r.get(3)?,
                password: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(DataPart {
        google,
        caldav,
        calendars,
        local: load_events(conn, Place::Local)?,
        outbox: load_events(conn, Place::Outbox)?,
        pending_deletes,
        settings: prefs,
    })
}

fn load_cache(conn: &Connection) -> Result<CachePart> {
    let sync_state = conn
        .prepare("SELECT calendar_id, token, full_sync_on, window_from FROM cache.sync_state")?
        .query_map([], |r| Ok((r.get(0)?, SyncState { token: r.get(1)?, full_sync_on: r.get(2)?, window_from: r.get(3)? })))?
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
        attendees: json_column(r, 16)?.unwrap_or_default(),
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
    let attendees = serde_json::to_string(&ev.attendees)?;
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {} ({EVENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
        attendees,
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
    match serde_json::from_str::<Store>(&text) {
        Ok(mut store) => {
            // Its sync state was keyed by Google calendar id.
            store.sync_state =
                store.sync_state.into_iter().map(|(id, s)| (CalRef::google(&id).local_id(), s)).collect();
            Some(store)
        }
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
    /// Fills in the refresh token and CalDAV passwords.
    fn load_secrets(&mut self, data: &mut DataPart) {
        let token = data.google.refresh_token.take();
        data.google.refresh_token =
            self.load_secret(REFRESH_TOKEN, token, "DELETE FROM main.settings WHERE key = 'google.refresh_token'", "");
        for a in &mut data.caldav {
            let pw = a.password.take();
            a.password =
                self.load_secret(&password_key(&a.id), pw, "UPDATE main.accounts SET password = NULL WHERE id = ?1", &a.id);
        }
    }

    /// Reads one secret. One found in `data.db` (saved by an older version,
    /// or because the credential store failed last time) is the newest; it
    /// is moved into the credential store when possible (`forget` with
    /// `param` removes it from `data.db`).
    fn load_secret(&mut self, key: &str, in_db: Option<String>, forget: &str, param: &str) -> Option<String> {
        if let Some(secret) = in_db {
            match self.secrets.set(key, Some(&secret)) {
                Ok(()) => {
                    let removed = if forget.contains("?1") {
                        self.conn.execute(forget, [param])
                    } else {
                        self.conn.execute(forget, [])
                    };
                    if let Err(e) = removed {
                        eprintln!("Could not remove {key} from data.db: {e}");
                        self.in_db.insert(key.to_owned());
                    }
                }
                Err(e) => {
                    eprintln!("Credential store unavailable ({e:#}); {key} stays in data.db");
                    self.in_db.insert(key.to_owned());
                }
            }
            return Some(secret);
        }
        match self.secrets.get(key) {
            Ok(secret) => secret,
            Err(e) => {
                eprintln!("Could not read {key} from the credential store: {e:#}");
                None
            }
        }
    }

    /// Writes a secret to the credential store if it `changed` (or is still
    /// waiting in `data.db`). Returns whether it must be kept in `data.db`
    /// instead, because that failed.
    fn store_secret(&mut self, key: &str, secret: Option<&String>, changed: bool) -> bool {
        if changed || self.in_db.contains(key) {
            match self.secrets.set(key, secret.map(String::as_str)) {
                Ok(()) => {
                    self.in_db.remove(key);
                }
                Err(e) if secret.is_some() => {
                    eprintln!("Credential store unavailable ({e:#}); keeping {key} in data.db");
                    self.in_db.insert(key.to_owned());
                }
                Err(e) => {
                    eprintln!("Could not remove {key} from the credential store: {e:#}");
                    self.in_db.remove(key);
                }
            }
        }
        self.in_db.contains(key)
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
        self.saved.caldav = data.caldav.clone();
        self.saved.pending_deletes = data.pending_deletes.clone();
        self.saved.sync_state = cache.sync_state.clone();
        self.saved.settings = data.settings.clone();

        Store {
            calendars: data.calendars,
            events,
            google: data.google,
            caldav: data.caldav,
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
        let remote_cals: HashSet<&str> = s.calendars.iter().filter(|c| c.is_remote()).map(|c| c.id.as_str()).collect();
        let place = |e: &Event| {
            if !remote_cals.contains(e.calendar_id.as_str()) {
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
        let caldav_changed = s.caldav != self.saved.caldav;
        let deletes_changed = s.pending_deletes != self.saved.pending_deletes;
        let sync_changed = s.sync_state != self.saved.sync_state;
        let settings_changed = s.settings != self.saved.settings;

        if upserts.is_empty()
            && deletes.is_empty()
            && !calendars_changed
            && !google_changed
            && !caldav_changed
            && !deletes_changed
            && !sync_changed
            && !settings_changed
        {
            return Ok(());
        }

        let token_changed = s.google.refresh_token != self.saved.google.refresh_token;
        let token_in_db = google_changed && self.store_secret(REFRESH_TOKEN, s.google.refresh_token.as_ref(), token_changed);
        let mut passwords_in_db = HashSet::new();
        if caldav_changed {
            let saved = std::mem::take(&mut self.saved.caldav);
            for a in &s.caldav {
                let before = saved.iter().find(|b| b.id == a.id).and_then(|b| b.password.as_ref());
                let changed = before != a.password.as_ref();
                if self.store_secret(&password_key(&a.id), a.password.as_ref(), changed) {
                    passwords_in_db.insert(a.id.clone());
                }
            }
            // Removed accounts take their passwords along.
            for gone in saved.iter().filter(|b| !s.caldav.iter().any(|a| a.id == b.id)) {
                self.store_secret(&password_key(&gone.id), None, true);
            }
            self.saved.caldav = saved;
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
                let remote = c.remote();
                tx.execute(
                    "INSERT INTO main.calendars (id, position, name, color, visible, remote_id, read_only, account)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        c.id,
                        i as i64,
                        c.name,
                        color,
                        c.visible,
                        remote.as_ref().map(|r| &r.id),
                        c.read_only,
                        remote.as_ref().map(|r| &r.account)
                    ],
                )?;
            }
        }
        if caldav_changed {
            tx.execute("DELETE FROM main.accounts", [])?;
            for (i, a) in s.caldav.iter().enumerate() {
                let password = a.password.as_ref().filter(|_| passwords_in_db.contains(&a.id));
                tx.execute(
                    "INSERT INTO main.accounts (id, position, label, server, username, password)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![a.id, i as i64, a.label, a.server, a.username, password],
                )?;
            }
        }
        if deletes_changed {
            tx.execute("DELETE FROM main.pending_deletes", [])?;
            for (i, d) in s.pending_deletes.iter().enumerate() {
                tx.execute(
                    "INSERT INTO main.pending_deletes (position, account, calendar_id, remote_id, etag)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![i as i64, d.calendar.account, d.calendar.id, d.remote_id, d.etag],
                )?;
            }
        }
        if sync_changed {
            tx.execute("DELETE FROM cache.sync_state", [])?;
            for (id, st) in &s.sync_state {
                tx.execute(
                    "INSERT INTO cache.sync_state (calendar_id, token, full_sync_on, window_from) VALUES (?1, ?2, ?3, ?4)",
                    params![id, st.token, st.full_sync_on, st.window_from],
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
        if caldav_changed {
            self.saved.caldav = s.caldav.clone();
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
            self.1.get(REFRESH_TOKEN).unwrap()
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
            attendees: Vec::new(),
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
            calendar: CalRef::google("me@example.com"),
            remote_id: "r9".into(),
            etag: Some("\"9\"".into()),
        });
        store.sync_state.insert(
            "g:me@example.com".into(),
            SyncState { token: Some("tok".into()), full_sync_on: Some(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()), window_from: Some(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()) },
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
    fn a_version_1_database_gains_invitees() {
        let dir = TempDir::new();
        drop(populated(&dir));
        // Turn data.db back into the version-1 layout (no attendees column).
        let conn = Connection::open(&dir.paths().data).unwrap();
        conn.execute_batch(
            "ALTER TABLE events DROP COLUMN attendees;
             ALTER TABLE outbox DROP COLUMN attendees;
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(conn);

        let mut store = open(&dir.paths());
        let ids: Vec<_> = sorted(&store.events).iter().map(|e| e.id.clone()).collect();
        assert!(ids.contains(&"local1".to_owned()) && ids.contains(&"g2".to_owned()), "local data kept: {ids:?}");
        assert!(store.events.iter().all(|e| e.attendees.is_empty()));
        assert_eq!(store.google.refresh_token.as_deref(), Some("refresh"));

        // Invitees are saved from now on.
        let ev = store.event_mut("local1").unwrap();
        ev.attendees.push(crate::model::Attendee::new("ann@example.com"));
        store.save();
        drop(store);
        let store = open(&dir.paths());
        assert_eq!(store.event("local1").unwrap().attendees[0].email, "ann@example.com");
        let conn = Connection::open(&dir.paths().data).unwrap();
        assert_eq!(user_version(&conn, "main").unwrap(), DATA_VERSION);
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
        again.sync_state.insert("g:me@example.com".into(), SyncState::default());
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
        assert_eq!(s.get("k").unwrap(), None);
        s.set("k", Some("secret")).unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("secret"));
        s.set("k", None).unwrap();
        assert_eq!(s.get("k").unwrap(), None);
        s.set("k", None).unwrap(); // deleting twice is fine
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
        assert_eq!(again.sync_state["google:me@example.com"].token.as_deref(), Some("t"));
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

    // ---- CalDAV accounts ------------------------------------------------------

    const DAV_CAL: &str = "https://p01-caldav.icloud.com/1234/calendars/home/";

    fn icloud() -> CalDavAccount {
        CalDavAccount {
            id: "caldav-1a2b".into(),
            label: "iCloud".into(),
            server: crate::caldav::ICLOUD.into(),
            username: "me@icloud.com".into(),
            password: Some("abcd-efgh-ijkl-mnop".into()),
        }
    }

    /// A signed-in iCloud account with a calendar, a cached event, an
    /// unsynced edit, a pending delete and a sync token.
    fn with_icloud(dir: &TempDir) -> Store {
        let mut store = open(&dir.paths());
        store.caldav.push(icloud());
        let r = CalRef::new("caldav-1a2b", DAV_CAL);
        store.calendars.push(Calendar {
            id: r.local_id(),
            name: "Home".into(),
            color: [255, 41, 104],
            visible: true,
            source: Source::CalDav { account: "caldav-1a2b".into(), url: DAV_CAL.into() },
            read_only: false,
        });
        let mut cached = event(&r.event_id("/1234/calendars/home/a.ics"), &r.local_id(), "Cached");
        cached.remote_id = Some("/1234/calendars/home/a.ics".into());
        let mut edited = event(&r.event_id("/1234/calendars/home/s.ics#20261116T070000"), &r.local_id(), "Edited");
        edited.remote_id = Some("/1234/calendars/home/s.ics#20261116T070000".into());
        edited.recurring_id = Some("/1234/calendars/home/s.ics".into());
        edited.pending = true;
        store.events.extend([cached, edited]);
        store.pending_deletes.push(PendingDelete { calendar: r.clone(), remote_id: "/1234/calendars/home/b.ics".into(), etag: None });
        store.sync_state.insert(r.local_id(), SyncState { token: Some("https://x/sync/1".into()), full_sync_on: None, window_from: None });
        store.save();
        store
    }

    #[test]
    fn caldav_accounts_round_trip_with_the_password_in_the_credential_store() {
        let dir = TempDir::new();
        let store = with_icloud(&dir);
        let (calendars, events, caldav, deletes, sync) = (
            store.calendars.clone(),
            sorted(&store.events),
            store.caldav.clone(),
            store.pending_deletes.clone(),
            store.sync_state.clone(),
        );
        drop(store);
        assert_eq!(dir.1.get("caldav-1a2b-password").unwrap().as_deref(), Some("abcd-efgh-ijkl-mnop"));
        let conn = Connection::open(dir.paths().data).unwrap();
        let in_db: Option<String> = conn.query_row("SELECT password FROM accounts", [], |r| r.get(0)).unwrap();
        assert_eq!(in_db, None, "no password in data.db");
        drop(conn);

        let again = open(&dir.paths());
        assert_eq!(again.caldav, caldav);
        assert_eq!(again.calendars, calendars);
        assert_eq!(sorted(&again.events), events);
        assert_eq!(again.pending_deletes, deletes);
        assert_eq!(again.sync_state, sync);
        // Synced events are split between cache and outbox like Google's.
        assert_eq!(rows(&dir.paths().data, "outbox").len(), 1);
        assert_eq!(rows(&dir.paths().cache, "events").len(), 1);
    }

    #[test]
    fn removing_an_account_forgets_its_password() {
        let dir = TempDir::new();
        let mut store = with_icloud(&dir);
        store.caldav.clear();
        store.save();
        assert_eq!(dir.1.get("caldav-1a2b-password").unwrap(), None);
        drop(store);
        assert!(open(&dir.paths()).caldav.is_empty());
    }

    #[test]
    fn a_changed_password_replaces_the_old_one() {
        let dir = TempDir::new();
        let mut store = with_icloud(&dir);
        store.caldav[0].password = Some("new-app-password".into());
        store.save();
        drop(store);
        assert_eq!(open(&dir.paths()).caldav[0].password.as_deref(), Some("new-app-password"));
    }

    #[test]
    fn without_a_credential_store_passwords_stay_in_data_db() {
        let dir = TempDir::new();
        let broken = Paths { secrets: Secrets::Unavailable, ..dir.paths() };
        let mut store = open(&broken);
        store.caldav.push(icloud());
        store.save();
        drop(store);
        assert_eq!(open(&broken).caldav[0].password.as_deref(), Some("abcd-efgh-ijkl-mnop"));

        // A working credential store takes it over.
        let store = open(&dir.paths());
        assert_eq!(store.caldav[0].password.as_deref(), Some("abcd-efgh-ijkl-mnop"));
        assert_eq!(dir.1.get("caldav-1a2b-password").unwrap().as_deref(), Some("abcd-efgh-ijkl-mnop"));
        drop(store);
        let conn = Connection::open(dir.paths().data).unwrap();
        let in_db: Option<String> = conn.query_row("SELECT password FROM accounts", [], |r| r.get(0)).unwrap();
        assert_eq!(in_db, None);
    }

    #[test]
    fn a_version_2_data_file_is_upgraded_to_accounts() {
        let dir = TempDir::new();
        let p = dir.paths();
        {
            // The tables as version 2 had them, with a Google calendar and a
            // pending Google delete.
            let conn = Connection::open(&p.data).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE calendars (id TEXT PRIMARY KEY, position INTEGER NOT NULL, name TEXT NOT NULL,
                     color INTEGER NOT NULL, visible INTEGER NOT NULL, google_id TEXT, read_only INTEGER NOT NULL);
                 {} {}
                 CREATE TABLE pending_deletes (position INTEGER PRIMARY KEY, google_calendar_id TEXT NOT NULL,
                     remote_id TEXT NOT NULL, etag TEXT);
                 INSERT INTO calendars VALUES ('local:home', 0, 'Home', 255, 1, NULL, 0);
                 INSERT INTO calendars VALUES ('google:me@example.com', 1, 'Me', 65280, 1, 'me@example.com', 0);
                 INSERT INTO pending_deletes VALUES (0, 'me@example.com', 'r9', '\"9\"');
                 INSERT INTO settings VALUES ('google.client_id', 'cid');
                 PRAGMA user_version = 2;",
                event_table("events"),
                event_table("outbox"),
            ))
            .unwrap();
        }
        let store = open(&p);
        assert_eq!(user_version(&Connection::open(&p.data).unwrap(), "main").unwrap(), DATA_VERSION);
        assert_eq!(store.calendars[0].source, Source::Local);
        assert_eq!(store.calendars[1].source, Source::Google { calendar_id: "me@example.com".into() });
        assert_eq!(store.calendars[1].remote(), Some(CalRef::google("me@example.com")));
        assert_eq!(
            store.pending_deletes,
            [PendingDelete { calendar: CalRef::google("me@example.com"), remote_id: "r9".into(), etag: Some("\"9\"".into()) }]
        );
        assert_eq!(store.google.client_id, "cid");
        drop(store);
        // And it keeps working after the upgrade.
        let mut again = open(&p);
        again.caldav.push(icloud());
        again.save();
        drop(again);
        assert_eq!(open(&p).caldav.len(), 1);
    }
}
