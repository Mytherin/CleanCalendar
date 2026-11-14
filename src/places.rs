//! Location suggestions that never leave this computer.
//!
//! In Settings → Locations the user adds the countries they want. Each
//! one's OpenStreetMap extract is downloaded
//! from Geofabrik, read into a small compressed index ([`index`]), and the
//! extract is deleted again. Typing a location in the event editor then
//! searches those indexes locally: no server ever sees what is typed.
//!
//! The editor also offers to open a location in Google Maps, which is just a
//! link the user chooses to follow.

#[cfg(feature = "build-places")]
pub mod build;
pub mod index;
#[cfg(feature = "build-places")]
pub mod osm;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use index::{Index, Meta, Query};

/// Where the prebuilt country indexes are published: the files of a GitHub
/// release. `CLEANCALENDAR_PLACES_URL` points elsewhere (e.g. a test
/// server).
const PLACES_URL: &str = "https://github.com/Mytherin/CleanCalendar/releases/download/v0.1/";
const PLACES_ENV: &str = "CLEANCALENDAR_PLACES_URL";
/// The published countries, built into the app: the `manifest.json` that
/// the last `--build-places` run wrote, copied here when its files are
/// released.
const COUNTRIES: &str = include_str!("places/countries.json");
/// No personal details, just the app.
const USER_AGENT: &str = concat!("CleanCalendar/", env!("CARGO_PKG_VERSION"));
pub const EXT: &str = "places";
pub const MAX_SUGGESTIONS: usize = 6;

// ---------------------------------------------------------------------------
// Text

/// Lowercase, without accents: "Café" and "cafe" match.
fn fold(c: char, out: &mut String) {
    let s = match c {
        'à'..='å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' => "d",
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĳ' => "ij",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ß' => "ss",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'þ' => "th",
        'ð' => "d",
        _ => {
            out.push(c);
            return;
        }
    };
    out.push_str(s);
}

/// The search words in `text`: lowercase, accents removed, split at
/// anything that isn't a letter or digit.
pub fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| {
        let mut out = String::with_capacity(w.len());
        for c in w.chars().flat_map(char::to_lowercase) {
            fold(c, &mut out);
        }
        out
    })
}

// ---------------------------------------------------------------------------
// Suggestions

/// One suggested place: "Rijksmuseum" and "Museumstraat 1, Amsterdam".
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub title: String,
    pub detail: String,
}

impl Suggestion {
    /// What goes in the event's location field.
    pub fn label(&self) -> String {
        if self.detail.is_empty() {
            self.title.clone()
        } else {
            format!("{}, {}", self.title, self.detail)
        }
    }
}

/// The Google Maps search page for a location.
pub fn google_maps_url(location: &str) -> String {
    let mut url = Url::parse("https://www.google.com/maps/search/").unwrap();
    url.query_pairs_mut().append_pair("api", "1").append_pair("query", location.trim());
    url.into()
}

// ---------------------------------------------------------------------------
// Regions on offer

/// A country whose prebuilt index is published, as `manifest.json` lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    /// Geofabrik's id: "netherlands", "us", …
    pub id: String,
    pub name: String,
    /// The index's file name next to the manifest.
    pub file: String,
    pub bytes: u64,
    pub entries: u64,
    /// When the map data was downloaded (YYYY-MM-DD).
    pub updated: String,
    /// Of the file, hex; checked after downloading.
    pub sha256: String,
}

/// The list of published countries.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub countries: Vec<Region>,
}

fn http() -> Result<reqwest::blocking::Client> {
    // No overall timeout: a big country can take a while to download.
    Ok(reqwest::blocking::Client::builder().user_agent(USER_AGENT).connect_timeout(Duration::from_secs(20)).build()?)
}

/// `base` with a trailing slash, so file names join onto it.
fn base_url() -> String {
    let base = std::env::var(PLACES_ENV).ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| PLACES_URL.into());
    if base.ends_with('/') {
        base
    } else {
        format!("{base}/")
    }
}

/// The countries built into the app, by name.
fn builtin_countries() -> Vec<Region> {
    // Checked by a test, so a bad list never ships.
    let mut list = serde_json::from_str::<Manifest>(COUNTRIES).map(|m| m.countries).unwrap_or_default();
    list.sort_by_key(|r| r.name.to_lowercase());
    list
}

pub fn sha256_hex(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Installed regions and downloads

/// Where the region files live: next to the cache database, since they can
/// always be downloaded again.
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLEANCALENDAR_DATA") {
        return PathBuf::from(d).join("places");
    }
    match directories::BaseDirs::new() {
        Some(b) => b.cache_dir().join(crate::db::APP_DIR).join("places"),
        None => PathBuf::from("places"),
    }
}

pub fn file_name(id: &str) -> String {
    format!("{}.{EXT}", id.replace(['/', '\\'], "_"))
}

/// A region on disk.
#[derive(Clone, Debug)]
pub struct Installed {
    pub meta: Meta,
    pub path: PathBuf,
    pub bytes: u64,
}

/// What a download is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Downloading { done: u64, total: Option<u64> },
    Failed(String),
}

struct Job {
    region: Region,
    cancel: AtomicBool,
    downloaded: AtomicU64,
    size: AtomicU64,
    failed: Mutex<Option<String>>,
}

impl Job {
    fn phase(&self) -> Phase {
        if let Some(e) = self.failed.lock().unwrap().clone() {
            return Phase::Failed(e);
        }
        let size = self.size.load(Ordering::Relaxed);
        Phase::Downloading { done: self.downloaded.load(Ordering::Relaxed), total: (size > 0).then_some(size) }
    }
}

/// A download shown in Settings.
#[derive(Clone, Debug)]
pub struct JobStatus {
    pub region: Region,
    pub phase: Phase,
}

/// The regions on disk, those being downloaded, and the loaded indexes.
pub struct Library {
    dir: PathBuf,
    /// Where the indexes are published (ends in '/').
    base: String,
    /// The countries on offer, by name.
    catalog: Vec<Region>,
    installed: Mutex<Option<Vec<Installed>>>,
    loaded: Mutex<HashMap<PathBuf, Arc<Index>>>,
    loading: AtomicBool,
    jobs: Mutex<Vec<Arc<Job>>>,
    /// Asks the UI to redraw when something changes in the background.
    repaint: Mutex<Option<eframe::egui::Context>>,
}

/// The app's one library.
pub fn library() -> &'static Library {
    static LIB: OnceLock<Library> = OnceLock::new();
    LIB.get_or_init(|| Library::new(dir(), base_url(), builtin_countries()))
}

impl Library {
    pub fn new(dir: PathBuf, base: String, catalog: Vec<Region>) -> Library {
        Library {
            dir,
            base,
            catalog,
            installed: Mutex::new(None),
            loaded: Mutex::new(HashMap::new()),
            loading: AtomicBool::new(false),
            jobs: Mutex::new(Vec::new()),
            repaint: Mutex::new(None),
        }
    }

    pub fn set_context(&self, ctx: &eframe::egui::Context) {
        let mut r = self.repaint.lock().unwrap();
        if r.is_none() {
            *r = Some(ctx.clone());
        }
    }

    fn repaint(&self) {
        if let Some(ctx) = self.repaint.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    /// The regions on disk, by name.
    pub fn installed(&self) -> Vec<Installed> {
        let mut cached = self.installed.lock().unwrap();
        if cached.is_none() {
            let mut list: Vec<Installed> = std::fs::read_dir(&self.dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == EXT))
                .filter_map(|path| {
                    let meta = index::read_meta(&path).ok()?;
                    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    Some(Installed { meta, path, bytes })
                })
                .collect();
            list.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
            *cached = Some(list);
        }
        cached.clone().unwrap()
    }

    /// Any region is installed (cheap; for every frame).
    pub fn has_regions(&self) -> bool {
        if let Some(list) = self.installed.lock().unwrap().as_ref() {
            return !list.is_empty();
        }
        !self.installed().is_empty()
    }

    /// A region is being downloaded or indexed.
    pub fn is_downloading(&self) -> bool {
        self.jobs.lock().unwrap().iter().any(|j| j.failed.lock().unwrap().is_none())
    }

    fn rescan(&self) {
        *self.installed.lock().unwrap() = None;
        let keep: Vec<PathBuf> = self.installed().into_iter().map(|i| i.path).collect();
        self.loaded.lock().unwrap().retain(|p, _| keep.contains(p));
        self.repaint();
    }

    /// Loads the installed regions in the background, so the first search
    /// doesn't wait for them.
    pub fn warm(&'static self) {
        if self.installed().is_empty() || self.loading.swap(true, Ordering::AcqRel) {
            return;
        }
        std::thread::spawn(move || {
            let _ = self.indexes();
            self.loading.store(false, Ordering::Release);
        });
    }

    /// Every installed region's index, loading any not loaded yet.
    fn indexes(&self) -> Vec<Arc<Index>> {
        let mut out = Vec::new();
        for inst in self.installed() {
            if let Some(idx) = self.loaded.lock().unwrap().get(&inst.path) {
                out.push(idx.clone());
                continue;
            }
            match Index::load(&inst.path) {
                Ok(idx) => {
                    let idx = Arc::new(idx);
                    self.loaded.lock().unwrap().insert(inst.path.clone(), idx.clone());
                    out.push(idx);
                }
                Err(e) => eprintln!("Could not load {}: {e:#}", inst.path.display()),
            }
        }
        out
    }

    /// The best places for what's typed, across all installed regions.
    pub fn search(&self, text: &str) -> Vec<Suggestion> {
        let q = Query::parse(text);
        let mut hits: Vec<index::Hit> = self.indexes().iter().flat_map(|idx| idx.search(&q, MAX_SUGGESTIONS)).collect();
        hits.sort_by_key(|h| h.rank);
        let mut out: Vec<Suggestion> = Vec::new();
        for h in hits {
            if !out.contains(&h.suggestion) {
                out.push(h.suggestion);
            }
        }
        out.truncate(MAX_SUGGESTIONS);
        out
    }

    pub fn remove(&self, inst: &Installed) -> Result<()> {
        self.loaded.lock().unwrap().remove(&inst.path);
        std::fs::remove_file(&inst.path).with_context(|| format!("removing {}", inst.path.display()))?;
        self.rescan();
        Ok(())
    }

    // ---- The catalog

    /// The countries on offer, by name.
    pub fn catalog(&self) -> &[Region] {
        &self.catalog
    }

    // ---- Downloads

    pub fn jobs(&self) -> Vec<JobStatus> {
        self.jobs.lock().unwrap().iter().map(|j| JobStatus { region: j.region.clone(), phase: j.phase() }).collect()
    }

    /// Downloads and installs `region` (again, if it's already installed).
    pub fn add(&'static self, region: Region) {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.iter().any(|j| j.region.id == region.id && j.failed.lock().unwrap().is_none()) {
            return;
        }
        jobs.retain(|j| j.region.id != region.id);
        let job = Arc::new(Job {
            region,
            cancel: AtomicBool::new(false),
            downloaded: AtomicU64::new(0),
            size: AtomicU64::new(0),
            failed: Mutex::new(None),
        });
        jobs.push(job.clone());
        drop(jobs);
        std::thread::spawn(move || {
            // Keep the UI moving while the job runs.
            let ticker = {
                let job = job.clone();
                std::thread::spawn(move || {
                    while Arc::strong_count(&job) > 2 {
                        self.repaint();
                        std::thread::sleep(Duration::from_millis(250));
                    }
                })
            };
            let result = self.run(&job);
            let cancelled = job.cancel.load(Ordering::Relaxed);
            match result {
                Ok(()) => self.jobs.lock().unwrap().retain(|j| !Arc::ptr_eq(j, &job)),
                Err(_) if cancelled => self.jobs.lock().unwrap().retain(|j| !Arc::ptr_eq(j, &job)),
                Err(e) => *job.failed.lock().unwrap() = Some(format!("{e:#}")),
            }
            drop(job);
            let _ = ticker.join();
            self.rescan();
        });
    }

    /// Stops a download, or forgets a failed one.
    pub fn cancel(&self, id: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        for j in jobs.iter() {
            if j.region.id == id {
                j.cancel.store(true, Ordering::Relaxed);
            }
        }
        jobs.retain(|j| j.region.id != id || j.failed.lock().unwrap().is_none());
    }

    fn run(&self, job: &Job) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let target = self.dir.join(file_name(&job.region.id));
        let part = target.with_extension("part");
        let result = self.download_and_install(job, &part, &target);
        let _ = std::fs::remove_file(&part);
        result
    }

    fn download_and_install(&self, job: &Job, part: &Path, target: &Path) -> Result<()> {
        let r = &job.region;
        // The manifest only names files next to it.
        if r.file.contains(['/', '\\']) || r.file.starts_with('.') {
            bail!("bad file name in the manifest: {}", r.file);
        }
        download(&format!("{}{}", self.base, r.file), part, &job.downloaded, &job.size, &job.cancel)?;
        if !sha256_hex(part)?.eq_ignore_ascii_case(&r.sha256) {
            bail!("the download of {} was damaged; please try again", r.name);
        }
        let meta = index::read_meta(part).context("reading the downloaded index")?;
        if meta.id != r.id {
            bail!("the download of {} holds {} instead", r.name, meta.name);
        }
        // Replacing an installed country: let go of the loaded copy first.
        self.loaded.lock().unwrap().remove(target);
        std::fs::rename(part, target).with_context(|| format!("saving {}", target.display()))
    }
}

/// Downloads `url` to `to`, counting bytes into `done` (of `size`, once
/// known); stops with an error when `cancel` is set.
pub fn download(url: &str, to: &Path, done: &AtomicU64, size: &AtomicU64, cancel: &AtomicBool) -> Result<()> {
    // Short messages for the Settings window; the URL isn't one the user
    // can do anything with.
    let mut resp = http()?.get(url).send().map_err(|e| {
        anyhow!(if e.is_timeout() {
            "Could not download: the connection timed out."
        } else if e.is_connect() {
            "Could not download: no connection to the server. Are you online?"
        } else {
            "Could not download: the connection failed."
        })
    })?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("Could not download: this country's location data isn't published yet.");
    }
    if !status.is_success() {
        bail!("Could not download: the server answered {status}.");
    }
    size.store(resp.content_length().unwrap_or(0), Ordering::Relaxed);
    let mut out = std::io::BufWriter::new(File::create(to).with_context(|| format!("creating {}", to.display()))?);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = resp.read(&mut buf).context("downloading")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("saving the download (is the disk full?)")?;
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    out.flush()?;
    let expected = size.load(Ordering::Relaxed);
    if expected > 0 && done.load(Ordering::Relaxed) != expected {
        return Err(anyhow!("the download was cut short"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The editor's lookup

/// How long typing must pause before searching.
const DEBOUNCE: Duration = Duration::from_millis(120);
const MIN_CHARS: usize = 2;

/// Suggestions for what's being typed into a location field. Searches run
/// on a background thread; answers to older text are dropped.
pub struct Lookup {
    pending: Option<(String, Instant)>,
    asked: Option<String>,
    /// The text the current suggestions are for.
    pub shown_for: String,
    pub suggestions: Vec<Suggestion>,
    /// The highlighted suggestion (arrow keys).
    pub selected: Option<usize>,
    tx: Sender<(String, Vec<Suggestion>)>,
    rx: Receiver<(String, Vec<Suggestion>)>,
}

impl Default for Lookup {
    fn default() -> Self {
        let (tx, rx) = channel();
        Lookup { pending: None, asked: None, shown_for: String::new(), suggestions: Vec::new(), selected: None, tx, rx }
    }
}

impl Lookup {
    /// The user typed: search for `text` once typing pauses.
    pub fn typed(&mut self, text: &str) {
        let text = text.trim();
        if text.chars().count() < MIN_CHARS {
            self.clear();
        } else {
            self.pending = Some((text.to_owned(), Instant::now()));
        }
    }

    /// Forget the suggestions (one was picked, or the field was left).
    pub fn clear(&mut self) {
        self.pending = None;
        self.asked = None;
        self.suggestions.clear();
        self.shown_for.clear();
        self.selected = None;
    }

    /// Starts a search once typing has paused, and takes in answers. Call
    /// every frame the field is shown.
    pub fn poll(&mut self, ctx: &eframe::egui::Context, lib: &'static Library) {
        while let Ok((query, found)) = self.rx.try_recv() {
            if self.asked.as_deref() == Some(query.as_str()) {
                self.suggestions = found;
                self.shown_for = query;
                self.selected = None;
            }
        }
        let Some((text, at)) = &self.pending else { return };
        let waited = at.elapsed();
        if waited < DEBOUNCE {
            ctx.request_repaint_after(DEBOUNCE - waited);
            return;
        }
        let query = text.clone();
        self.pending = None;
        if self.asked.as_deref() == Some(query.as_str()) {
            return;
        }
        self.asked = Some(query.clone());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let found = lib.search(&query);
            let _ = tx.send((query, found));
            ctx.request_repaint();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_countries_are_well_formed() {
        let m: Manifest = serde_json::from_str(COUNTRIES).expect("countries.json parses");
        assert!(!m.countries.is_empty());
        let mut ids = std::collections::HashSet::new();
        for r in &m.countries {
            assert!(ids.insert(&r.id), "{} twice", r.id);
            assert_eq!(r.file, file_name(&r.id), "{r:?}");
            assert!(r.sha256.len() == 64 && r.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{r:?}");
            assert!(r.bytes > 0 && r.entries > 0 && !r.name.is_empty(), "{r:?}");
        }
        assert_eq!(builtin_countries().len(), m.countries.len());
    }

    /// Against the real release: installs one country from the built-in
    /// list and searches it. `CLEANCALENDAR_TEST_COUNTRY=netherlands cargo
    /// test live_release -- --ignored`
    #[test]
    #[ignore]
    fn live_release() {
        let id = std::env::var("CLEANCALENDAR_TEST_COUNTRY").unwrap_or("monaco".into());
        let dir = std::env::temp_dir().join(format!("cc-live-{}", uuid::Uuid::new_v4()));
        let lib: &'static Library = Box::leak(Box::new(Library::new(dir.clone(), PLACES_URL.into(), builtin_countries())));
        let region = lib.catalog().iter().find(|r| r.id == id).expect("in the built-in list").clone();
        lib.add(region);
        wait_for_long(|| !lib.is_downloading());
        assert!(lib.jobs().is_empty(), "{:?}", lib.jobs());
        wait_for_long(|| lib.has_regions());
        let inst = lib.installed();
        println!("installed {} ({} places, {} bytes)", inst[0].meta.name, inst[0].meta.entries, inst[0].bytes);
        let found = lib.search(&std::env::var("CLEANCALENDAR_TEST_QUERY").unwrap_or("casino".into()));
        println!("{found:#?}");
        assert!(!found.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wait_for_long(mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(300), "timed out");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn words_are_folded() {
        let t: Vec<String> = tokens("Café-de Jaren, Straße ŁÓDŹ 12a").collect();
        assert_eq!(t, ["cafe", "de", "jaren", "strasse", "lodz", "12a"]);
    }

    #[test]
    fn google_maps_link_escapes_the_location() {
        assert_eq!(
            google_maps_url(" Café de Paris, 12 Rue & Co "),
            "https://www.google.com/maps/search/?api=1&query=Caf%C3%A9+de+Paris%2C+12+Rue+%26+Co"
        );
    }

    #[test]
    fn lookup_waits_for_enough_text() {
        let mut l = Lookup::default();
        l.typed("a");
        assert!(l.pending.is_none());
        l.typed(" am ");
        assert_eq!(l.pending.as_ref().map(|(t, _)| t.as_str()), Some("am"));
    }

    /// Serves `files` (path -> body) over HTTP on a local port, until the
    /// test ends; answers 404 for anything else.
    fn serve(files: HashMap<String, Vec<u8>>) -> u16 {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                // Skip the headers.
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut stream = stream;
                match files.get(&path) {
                    Some(body) => {
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        port
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The app's whole path against a stand-in for the GitHub release: the
    /// manifest, a download that is checked and installed, a damaged one
    /// that is refused, a search, and removing a country.
    #[test]
    fn installs_published_countries() {
        use index::{Kind, Meta, Record};
        let tmp = std::env::temp_dir().join(format!("cc-places-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let built = tmp.join("monaco.places");
        let meta = Meta { id: "monaco".into(), name: "Monaco".into(), country: "Monaco".into(), number_first: true, ..Default::default() };
        let casino = Record {
            kind: Kind::Poi,
            name: "Casino de Monte-Carlo".into(),
            street: "Place du Casino".into(),
            number: "2".into(),
            city: "Monaco".into(),
        };
        index::write(&built, meta, vec![casino]).unwrap();
        let body = std::fs::read(&built).unwrap();
        let region = |id: &str, name: &str, sha: String| Region {
            id: id.into(),
            name: name.into(),
            file: format!("{id}.places"),
            bytes: body.len() as u64,
            entries: 1,
            updated: "2026-11-14".into(),
            sha256: sha,
        };
        let countries = vec![
            region("monaco", "Monaco", sha256_hex(&built).unwrap()),
            // Its file doesn't match its checksum.
            region("andorra", "Andorra", "00".repeat(32)),
            // Not on the server at all.
            region("liechtenstein", "Liechtenstein", "00".repeat(32)),
        ];
        let files = HashMap::from([
            ("/rel/monaco.places".to_owned(), body.clone()),
            ("/rel/andorra.places".to_owned(), body.clone()),
        ]);
        let port = serve(files);
        let dir = tmp.join("installed");
        let mut list = countries;
        list.sort_by_key(|r| r.name.to_lowercase());
        let lib: &'static Library = Box::leak(Box::new(Library::new(dir.clone(), format!("http://127.0.0.1:{port}/rel/"), list)));

        let catalog = lib.catalog().to_vec();
        let names: Vec<&str> = catalog.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Andorra", "Liechtenstein", "Monaco"]);
        assert!(!lib.has_regions());

        lib.add(catalog[2].clone());
        wait_for("Monaco to install", || lib.has_regions() && !lib.is_downloading());
        let inst = lib.installed();
        assert_eq!((inst[0].meta.name.as_str(), inst[0].meta.entries), ("Monaco", 1));
        let found: Vec<String> = lib.search("casino").iter().map(Suggestion::label).collect();
        assert_eq!(found, ["Casino de Monte-Carlo, 2 Place du Casino, Monaco"]);

        lib.add(catalog[0].clone());
        wait_for("Andorra to fail", || matches!(lib.jobs().first().map(|j| j.phase.clone()), Some(Phase::Failed(_))));
        let Phase::Failed(e) = lib.jobs()[0].phase.clone() else { unreachable!() };
        assert!(e.contains("damaged"), "{e}");
        lib.cancel("andorra");
        assert!(lib.jobs().is_empty());

        // A short message, without the URL, for the Settings window.
        lib.add(catalog[1].clone());
        wait_for("Liechtenstein to fail", || matches!(lib.jobs().first().map(|j| j.phase.clone()), Some(Phase::Failed(_))));
        let Phase::Failed(e) = lib.jobs()[0].phase.clone() else { unreachable!() };
        assert_eq!(e, "Could not download: this country's location data isn't published yet.");
        lib.cancel("liechtenstein");

        // Only the installed index is left: no partial downloads.
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(files, ["monaco.places"]);
        lib.remove(&inst[0]).unwrap();
        assert!(!lib.has_regions());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
