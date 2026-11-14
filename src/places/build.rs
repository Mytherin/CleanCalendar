//! `clean-calendar --build-places <dir> [country …]`: builds the country
//! indexes the app downloads (only with `--features build-places`).
//!
//! For each country in Geofabrik's catalog (or just those named), the
//! OpenStreetMap extract is downloaded, read ([`super::osm`]), written as
//! `<country>.places` in `<dir>`, and deleted again. `manifest.json` in
//! `<dir>` lists the indexes built so far, with their sizes and checksums.
//! The `.places` files are then attached to the GitHub release, and the
//! manifest is copied to `src/places/countries.json`, which the app has
//! built in.
//!
//! Countries already in the manifest are skipped unless `--force` is given,
//! so an interrupted build picks up where it left off.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::index::{self, Meta};
use super::{download, file_name, http, osm, sha256_hex, Manifest, Region};

/// Lists the indexes built, next to them; copied to `src/places/countries.json`
/// to build the list into the app.
pub const MANIFEST: &str = "manifest.json";

/// Where Geofabrik lists its extracts. `CLEANCALENDAR_GEOFABRIK_URL`
/// points elsewhere (e.g. a mirror).
const CATALOG: &str = "https://download.geofabrik.de/index-v1-nogeom.json";
const CATALOG_ENV: &str = "CLEANCALENDAR_GEOFABRIK_URL";

/// A country Geofabrik has an extract of.
#[derive(Clone, Debug, PartialEq)]
pub struct Country {
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
/// countries are built (the entries with a country code), not continents or
/// a country's states and provinces.
pub fn parse_catalog(json: &Value) -> Vec<Country> {
    let mut out: Vec<Country> = json["features"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let p = &f["properties"];
            let id = p["id"].as_str()?;
            let codes: Vec<&str> = p["iso3166-1:alpha2"].as_array()?.iter().filter_map(Value::as_str).collect();
            let pbf = p["urls"]["pbf"].as_str()?;
            let name = p["name"].as_str().unwrap_or(id);
            (!codes.is_empty()).then(|| Country {
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

fn fetch_catalog() -> Result<Vec<Country>> {
    let url = std::env::var(CATALOG_ENV).ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| CATALOG.into());
    let json: Value = http()?.get(url).timeout(Duration::from_secs(60)).send()?.error_for_status()?.json()?;
    let list = parse_catalog(&json);
    if list.is_empty() {
        bail!("Geofabrik's catalog lists no countries");
    }
    Ok(list)
}

fn mb(n: u64) -> String {
    format!("{:.1} MB", n as f64 / 1_048_576.0)
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    match std::fs::read(dir.join(MANIFEST)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading manifest.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e.into()),
    }
}

fn write_manifest(dir: &Path, m: &mut Manifest) -> Result<()> {
    m.countries.sort_by_key(|r| r.name.to_lowercase());
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(&tmp, dir.join(MANIFEST))?;
    Ok(())
}

/// Prints `line()` every few seconds until dropped.
struct Ticker(std::sync::Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);

impl Ticker {
    fn start(line: impl Fn() -> String + Send + 'static) -> Ticker {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut last = Instant::now();
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                if last.elapsed() >= Duration::from_secs(5) {
                    println!("    {}", line());
                    last = Instant::now();
                }
            }
        });
        Ticker(stop, Some(handle))
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        if let Some(h) = self.1.take() {
            let _ = h.join();
        }
    }
}

/// Downloads, reads and writes one country; returns its manifest entry.
pub fn build_one(c: &Country, dir: &Path) -> Result<Region> {
    let file = file_name(&c.id);
    let target = dir.join(&file);
    let pbf = dir.join(format!("{file}.osm.pbf.part"));
    let cancel = AtomicBool::new(false);

    let started = Instant::now();
    let done = std::sync::Arc::new(AtomicU64::new(0));
    let size = std::sync::Arc::new(AtomicU64::new(0));
    {
        let (d, s) = (done.clone(), size.clone());
        let _t = Ticker::start(move || format!("downloaded {} of {}", mb(d.load(Ordering::Relaxed)), mb(s.load(Ordering::Relaxed))));
        download(&c.pbf, &pbf, None, &done, &size, &cancel)?;
    }
    let pbf_bytes = done.load(Ordering::Relaxed);

    let blocks = std::sync::Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
    let records = {
        let b = blocks.clone();
        let _t = Ticker::start(move || format!("read {} of {} blocks", b.0.load(Ordering::Relaxed), b.1.load(Ordering::Relaxed)));
        let progress = osm::Progress { done: &blocks.0, total: &blocks.1, cancel: &cancel };
        osm::read(&pbf, &progress)
    };
    let _ = std::fs::remove_file(&pbf);
    let records = records.context("reading the map data")?;

    let meta = Meta {
        id: c.id.clone(),
        name: c.name.clone(),
        country: c.country.clone(),
        number_first: c.number_first,
        updated: chrono::Local::now().date_naive().to_string(),
        entries: 0,
    };
    index::write(&target, meta, records).context("writing the index")?;
    let meta = index::read_meta(&target)?;
    let bytes = std::fs::metadata(&target)?.len();
    println!(
        "    {} -> {} ({} places) in {:.0?}",
        mb(pbf_bytes),
        mb(bytes),
        meta.entries,
        started.elapsed()
    );
    Ok(Region {
        id: c.id.clone(),
        name: c.name.clone(),
        file,
        bytes,
        entries: meta.entries,
        updated: meta.updated,
        sha256: sha256_hex(&target)?,
    })
}

/// Builds `ids` (or every country) into `dir`, keeping `manifest.json` up
/// to date after each one.
pub fn build(dir: &Path, ids: &[String], force: bool) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let catalog = fetch_catalog()?;
    let wanted: Vec<&Country> = if ids.is_empty() {
        catalog.iter().collect()
    } else {
        let mut out = Vec::new();
        for id in ids {
            match catalog.iter().find(|c| &c.id == id) {
                Some(c) => out.push(c),
                None => bail!("no country {id:?} in Geofabrik's catalog (try --list)"),
            }
        }
        out
    };
    let mut manifest = read_manifest(dir)?;
    let mut failed = Vec::new();
    for (i, c) in wanted.iter().enumerate() {
        let have = manifest.countries.iter().any(|r| r.id == c.id && dir.join(&r.file).exists());
        if have && !force {
            println!("[{}/{}] {}: already built", i + 1, wanted.len(), c.name);
            continue;
        }
        println!("[{}/{}] {} ({})", i + 1, wanted.len(), c.name, c.id);
        match build_one(c, dir) {
            Ok(entry) => {
                manifest.countries.retain(|r| r.id != entry.id);
                manifest.countries.push(entry);
                write_manifest(dir, &mut manifest)?;
            }
            Err(e) => {
                println!("    failed: {e:#}");
                failed.push(c.id.clone());
            }
        }
    }
    let total: u64 = manifest.countries.iter().map(|r| r.bytes).sum();
    println!("{} countries in {}, {} in all", manifest.countries.len(), dir.join(MANIFEST).display(), mb(total));
    if !failed.is_empty() {
        bail!("failed: {}", failed.join(", "));
    }
    Ok(())
}

const USAGE: &str = "usage: clean-calendar --build-places <dir> [--force] [country-id …]
       clean-calendar --build-places --list";

/// Runs `--build-places` if that's what was asked for; returns the exit code.
pub fn main_if_asked() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("--build-places") {
        return None;
    }
    // A release build has no console of its own: use the one it was run from.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
    let rest = &args[1..];
    let result = if rest.iter().any(|a| a == "--list") {
        fetch_catalog().map(|list| {
            for c in list {
                println!("{:<40} {}", c.id, c.name);
            }
        })
    } else {
        let force = rest.iter().any(|a| a == "--force");
        let plain: Vec<String> = rest.iter().filter(|a| !a.starts_with("--")).cloned().collect();
        match plain.split_first() {
            Some((dir, ids)) => build(&PathBuf::from(dir), ids, force),
            None => {
                eprintln!("{USAGE}");
                return Some(2);
            }
        }
    };
    match result {
        Ok(()) => Some(0),
        Err(e) => {
            eprintln!("error: {e:#}");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// Against the real Geofabrik: builds Monaco (under 1 MB) twice, the
    /// second time skipped. `cargo test --features build-places
    /// builds_monaco -- --ignored`
    #[test]
    #[ignore]
    fn builds_monaco() {
        let dir = std::env::temp_dir().join(format!("cc-build-{}", uuid::Uuid::new_v4()));
        build(&dir, &["monaco".into()], false).unwrap();
        let m = read_manifest(&dir).unwrap();
        assert_eq!(m.countries.len(), 1);
        let r = &m.countries[0];
        assert_eq!((r.id.as_str(), r.file.as_str()), ("monaco", "monaco.places"));
        assert!(r.entries > 500 && r.bytes < 200_000, "{r:?}");
        assert_eq!(sha256_hex(&dir.join(&r.file)).unwrap(), r.sha256);
        let idx = index::Index::load(&dir.join(&r.file)).unwrap();
        assert_eq!((idx.meta.country.as_str(), idx.meta.number_first), ("Monaco", true));
        // Only the index and the manifest are left.
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        files.sort();
        assert_eq!(files, ["manifest.json", "monaco.places"]);
        build(&dir, &["monaco".into()], false).unwrap();
        assert_eq!(read_manifest(&dir).unwrap(), m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds an index from an extract on disk and times a few searches:
    /// `CLEANCALENDAR_TEST_PBF=monaco-latest.osm.pbf cargo test --features
    /// build-places real_extract -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_extract() {
        use std::collections::HashMap;
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
        let n = records.len();
        let t = Instant::now();
        index::write(&out, Meta { id: "test".into(), name: "Test".into(), country: "Test".into(), ..Default::default() }, records).unwrap();
        let wrote = t.elapsed();
        let size = std::fs::metadata(&out).unwrap().len();
        let t = Instant::now();
        let idx = index::Index::load(&out).unwrap();
        println!("records: {n} {kinds:?}");
        println!("read {read:?}, wrote {wrote:?}, loaded {:?}", t.elapsed());
        println!("pbf {} bytes -> index {size} bytes", std::fs::metadata(&pbf).unwrap().len());
        let queries = std::env::var("CLEANCALENDAR_TEST_QUERIES").unwrap_or("monte carlo,casino,boulevard".into());
        for q in queries.split(',') {
            let t = Instant::now();
            let hits = idx.search(&index::Query::parse(q), super::super::MAX_SUGGESTIONS);
            println!("{q:?} ({:?}):", t.elapsed());
            for h in hits {
                println!("    {}", h.suggestion.label());
            }
        }
        let _ = std::fs::remove_file(&out);
    }
}
