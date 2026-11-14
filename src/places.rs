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

pub mod index;
pub mod osm;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use url::Url;

use index::{Index, Meta, Query};

/// Where Geofabrik lists its extracts. `CLEANCALENDAR_GEOFABRIK_URL`
/// points elsewhere (e.g. a mirror).
const CATALOG: &str = "https://download.geofabrik.de/index-v1-nogeom.json";
const CATALOG_ENV: &str = "CLEANCALENDAR_GEOFABRIK_URL";
/// No personal details, just the app.
const USER_AGENT: &str = concat!("CleanCalendar/", env!("CARGO_PKG_VERSION"));
const EXT: &str = "places";
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

/// A country Geofabrik has an extract of.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    /// "netherlands", "us", …
    pub id: String,
    pub name: String,
    /// Added to towns' suggestions ("Utrecht, Netherlands"); empty for a
    /// file covering several countries ("Haiti and Dominican Republic").
    pub country: String,
    pub number_first: bool,
    pub pbf: String,
}

/// Countries that write the house number before the street.
const NUMBER_FIRST: &[&str] = &["US", "CA", "GB", "IE", "AU", "NZ", "FR", "LU", "MC", "IN", "PH", "ZA", "SG", "MY", "HK", "TH", "IL"];

/// The countries in Geofabrik's catalog, sorted by name. Only whole
/// countries are offered (the entries with a country code), not continents
/// or a country's states and provinces.
pub fn parse_catalog(json: &Value) -> Vec<Region> {
    let mut out: Vec<Region> = json["features"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let p = &f["properties"];
            let id = p["id"].as_str()?;
            let codes: Vec<&str> = p["iso3166-1:alpha2"].as_array()?.iter().filter_map(Value::as_str).collect();
            let pbf = p["urls"]["pbf"].as_str()?;
            let name = p["name"].as_str().unwrap_or(id);
            (!codes.is_empty()).then(|| Region {
                id: id.to_owned(),
                name: name.to_owned(),
                country: if codes.len() == 1 { name.to_owned() } else { String::new() },
                number_first: codes.iter().any(|c| NUMBER_FIRST.contains(c)),
                pbf: pbf.to_owned(),
            })
        })
        .collect();
    out.sort_by_key(|r| r.name.to_lowercase());
    out
}

fn http() -> Result<reqwest::blocking::Client> {
    // No overall timeout: a country can take a while to download.
    Ok(reqwest::blocking::Client::builder().user_agent(USER_AGENT).connect_timeout(Duration::from_secs(20)).build()?)
}

fn fetch_catalog() -> Result<Vec<Region>> {
    let url = std::env::var(CATALOG_ENV).ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| CATALOG.into());
    let json: Value = http()?.get(url).timeout(Duration::from_secs(60)).send()?.error_for_status()?.json()?;
    let regions = parse_catalog(&json);
    if regions.is_empty() {
        bail!("the list of regions is empty");
    }
    Ok(regions)
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

fn file_name(id: &str) -> String {
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
    Indexing { done: usize, total: usize },
    Saving,
    Failed(String),
}

struct Job {
    region: Region,
    cancel: AtomicBool,
    downloaded: AtomicU64,
    size: AtomicU64,
    blocks_done: AtomicUsize,
    blocks_total: AtomicUsize,
    /// 0 downloading, 1 indexing, 2 saving.
    stage: AtomicUsize,
    failed: Mutex<Option<String>>,
}

impl Job {
    fn phase(&self) -> Phase {
        if let Some(e) = self.failed.lock().unwrap().clone() {
            return Phase::Failed(e);
        }
        match self.stage.load(Ordering::Relaxed) {
            0 => {
                let size = self.size.load(Ordering::Relaxed);
                Phase::Downloading { done: self.downloaded.load(Ordering::Relaxed), total: (size > 0).then_some(size) }
            }
            1 => Phase::Indexing {
                done: self.blocks_done.load(Ordering::Relaxed),
                total: self.blocks_total.load(Ordering::Relaxed),
            },
            _ => Phase::Saving,
        }
    }
}

/// A download shown in Settings.
#[derive(Clone, Debug)]
pub struct JobStatus {
    pub region: Region,
    pub phase: Phase,
}

#[derive(Default)]
enum Catalog {
    #[default]
    NotAsked,
    Loading,
    Ready(Vec<Region>),
    Failed(String),
}

/// The regions on disk, those being downloaded, and the loaded indexes.
pub struct Library {
    dir: PathBuf,
    installed: Mutex<Option<Vec<Installed>>>,
    loaded: Mutex<HashMap<PathBuf, Arc<Index>>>,
    loading: AtomicBool,
    jobs: Mutex<Vec<Arc<Job>>>,
    catalog: Mutex<Catalog>,
    /// Asks the UI to redraw when something changes in the background.
    repaint: Mutex<Option<eframe::egui::Context>>,
}

/// The app's one library.
pub fn library() -> &'static Library {
    static LIB: OnceLock<Library> = OnceLock::new();
    LIB.get_or_init(|| Library::new(dir()))
}

impl Library {
    pub fn new(dir: PathBuf) -> Library {
        Library {
            dir,
            installed: Mutex::new(None),
            loaded: Mutex::new(HashMap::new()),
            loading: AtomicBool::new(false),
            jobs: Mutex::new(Vec::new()),
            catalog: Mutex::new(Catalog::NotAsked),
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

    /// The regions on offer: `Ok(None)` while they load (asks for them the
    /// first time), `Err` if they couldn't be fetched.
    pub fn catalog(&'static self) -> Result<Option<Vec<Region>>, String> {
        let mut c = self.catalog.lock().unwrap();
        match &*c {
            Catalog::Ready(list) => Ok(Some(list.clone())),
            Catalog::Loading => Ok(None),
            Catalog::Failed(e) => Err(e.clone()),
            Catalog::NotAsked => {
                *c = Catalog::Loading;
                std::thread::spawn(move || {
                    let got = fetch_catalog();
                    *self.catalog.lock().unwrap() = match got {
                        Ok(list) => Catalog::Ready(list),
                        Err(e) => Catalog::Failed(format!("Could not get the list of regions: {e:#}")),
                    };
                    self.repaint();
                });
                Ok(None)
            }
        }
    }

    /// Fetch the catalog again next time (after it failed).
    pub fn retry_catalog(&self) {
        *self.catalog.lock().unwrap() = Catalog::NotAsked;
    }

    // ---- Downloads

    pub fn jobs(&self) -> Vec<JobStatus> {
        self.jobs.lock().unwrap().iter().map(|j| JobStatus { region: j.region.clone(), phase: j.phase() }).collect()
    }

    /// Downloads and indexes `region` (again, if it's already installed).
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
            blocks_done: AtomicUsize::new(0),
            blocks_total: AtomicUsize::new(0),
            stage: AtomicUsize::new(0),
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
        let pbf = target.with_extension("osm.pbf.part");
        let result = self.download_and_index(job, &pbf, &target);
        let _ = std::fs::remove_file(&pbf);
        result
    }

    fn download_and_index(&self, job: &Job, pbf: &Path, target: &Path) -> Result<()> {
        download(&job.region.pbf, pbf, &job.downloaded, &job.size, &job.cancel)?;
        job.stage.store(1, Ordering::Relaxed);
        let progress = osm::Progress { done: &job.blocks_done, total: &job.blocks_total, cancel: &job.cancel };
        let records = osm::read(pbf, &progress).context("reading the map data")?;
        if job.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        job.stage.store(2, Ordering::Relaxed);
        let meta = Meta {
            id: job.region.id.clone(),
            name: job.region.name.clone(),
            country: job.region.country.clone(),
            number_first: job.region.number_first,
            updated: chrono::Local::now().date_naive().to_string(),
            entries: 0,
        };
        // Replacing an installed region: let go of the loaded copy first.
        self.loaded.lock().unwrap().remove(target);
        index::write(target, meta, records).context("saving the index")
    }
}

fn download(url: &str, to: &Path, done: &AtomicU64, size: &AtomicU64, cancel: &AtomicBool) -> Result<()> {
    let mut resp = http()?.get(url).send()?.error_for_status().with_context(|| format!("downloading {url}"))?;
    size.store(resp.content_length().unwrap_or(0), Ordering::Relaxed);
    let mut out = std::io::BufWriter::new(File::create(to).with_context(|| format!("creating {}", to.display()))?);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = resp.read(&mut buf).context("downloading the map data")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("saving the map data (is the disk full?)")?;
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
    use serde_json::json;

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
    fn catalog_lists_only_countries() {
        let f = |id: &str, name: &str, iso: &[&str]| {
            let mut p = json!({"id": id, "name": name, "urls": {"pbf": format!("https://x/{id}.osm.pbf")}});
            if !iso.is_empty() {
                p["iso3166-1:alpha2"] = json!(iso);
            }
            json!({"properties": p})
        };
        let json = json!({"features": [
            f("europe", "Europe", &[]),
            f("netherlands", "Netherlands", &["NL"]),
            f("us", "United States of America", &["US"]),
            f("us/california", "California", &[]),
            f("bayern", "Bayern", &[]),
            f("haiti-and-domrep", "Haiti and Dominican Republic", &["HT", "DO"]),
        ]});
        let got = parse_catalog(&json);
        let rows: Vec<(&str, &str, bool)> = got.iter().map(|r| (r.name.as_str(), r.country.as_str(), r.number_first)).collect();
        assert_eq!(
            rows,
            [
                // Two countries in one file: towns don't get a country name.
                ("Haiti and Dominican Republic", "", false),
                ("Netherlands", "Netherlands", false),
                ("United States of America", "United States of America", true),
            ]
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

    /// The whole path against the real Geofabrik: the catalog, then a
    /// download of Monaco (under 1 MB), indexing, and a search.
    /// `cargo test geofabrik_download -- --ignored`
    #[test]
    #[ignore]
    fn geofabrik_download() {
        let dir = std::env::temp_dir().join(format!("cc-places-{}", uuid::Uuid::new_v4()));
        let lib: &'static Library = Box::leak(Box::new(Library::new(dir.clone())));
        let catalog = loop {
            match lib.catalog() {
                Ok(Some(c)) => break c,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("{e}"),
            }
        };
        assert!((150..300).contains(&catalog.len()), "{} countries", catalog.len());
        // Countries (and territories with their own code, like Puerto Rico),
        // not states or provinces.
        assert!(catalog.iter().any(|r| r.id == "us"));
        assert!(!catalog.iter().any(|r| r.id == "us/california" || r.id == "bayern"));
        let monaco = catalog.iter().find(|r| r.id == "monaco").unwrap().clone();
        assert_eq!((monaco.name.as_str(), monaco.country.as_str(), monaco.number_first), ("Monaco", "Monaco", true));
        assert!(!lib.has_regions());
        lib.add(monaco);
        let start = Instant::now();
        while lib.is_downloading() {
            assert!(start.elapsed() < Duration::from_secs(120), "{:?}", lib.jobs());
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(lib.jobs().is_empty(), "{:?}", lib.jobs());
        // The job's thread rescans just after it leaves the list.
        while !lib.has_regions() {
            assert!(start.elapsed() < Duration::from_secs(120));
            std::thread::sleep(Duration::from_millis(50));
        }
        let inst = lib.installed();
        assert_eq!(inst[0].meta.name, "Monaco");
        assert!(inst[0].meta.entries > 500 && inst[0].bytes < 200_000, "{inst:?}");
        let found = lib.search("casino");
        assert!(found.iter().any(|s| s.title.contains("Casino")), "{found:?}");
        // Nothing else is left behind.
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(files, ["monaco.places"]);
        lib.remove(&inst[0]).unwrap();
        assert!(!lib.has_regions());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds an index from a real extract and times a few searches:
    /// `CLEANCALENDAR_TEST_PBF=monaco-latest.osm.pbf cargo test real_extract -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_extract() {
        let pbf = PathBuf::from(std::env::var("CLEANCALENDAR_TEST_PBF").expect("set CLEANCALENDAR_TEST_PBF"));
        let (done, total, cancel) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicBool::new(false));
        let t = Instant::now();
        let records = osm::read(&pbf, &osm::Progress { done: &done, total: &total, cancel: &cancel }).unwrap();
        let read = t.elapsed();
        let mut kinds: HashMap<index::Kind, usize> = HashMap::new();
        for r in &records {
            *kinds.entry(r.kind).or_default() += 1;
        }
        let out = std::env::temp_dir().join(format!("cc-test-{}.places", uuid::Uuid::new_v4()));
        let t = Instant::now();
        let n = records.len();
        index::write(&out, Meta { id: "test".into(), name: "Test".into(), country: "Test".into(), ..Default::default() }, records).unwrap();
        let wrote = t.elapsed();
        let size = std::fs::metadata(&out).unwrap().len();
        let t = Instant::now();
        let idx = Index::load(&out).unwrap();
        let loaded = t.elapsed();
        println!("records: {n} {kinds:?}");
        println!("read {read:?}, wrote {wrote:?}, loaded {loaded:?}");
        println!("pbf {} bytes -> index {size} bytes", std::fs::metadata(&pbf).unwrap().len());
        for q in std::env::var("CLEANCALENDAR_TEST_QUERIES").unwrap_or("monte carlo,casino,boulevard,ams,rijksmuseum,damrak 70,utrecht cen".into()).split(',') {
            let t = Instant::now();
            let hits = idx.search(&Query::parse(q), MAX_SUGGESTIONS);
            println!("{q:?} ({:?}):", t.elapsed());
            for h in hits {
                println!("    {}", h.suggestion.label());
            }
        }
        let _ = std::fs::remove_file(&out);
    }
}
