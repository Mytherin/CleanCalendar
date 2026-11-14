//! The on-disk index of one region's places, built to be small.
//!
//! A `.places` file is a short header followed by sections, each compressed
//! with zstd:
//!
//! - the entries, stored by column: kind, name, street, house number and
//!   town. Each text column is dictionary-encoded (every distinct string is
//!   stored once, sorted so that neighbours share prefixes) and the entries
//!   are sorted by kind, town and street, so the code columns compress well;
//! - the search words, as an [`fst`] map from each normalized word to its
//!   posting list;
//! - the posting lists: for each word, the entries it occurs in, as
//!   delta-encoded varints. The lowest bit says whether the word is in the
//!   entry's own name (rather than its street or town).
//!
//! The whole file is read into memory when it is first searched; searching
//! is then a prefix walk of the word map plus a merge of posting lists.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use fst::{Automaton, IntoStreamer, Streamer};
use serde::{Deserialize, Serialize};

use super::{tokens, Suggestion};

const MAGIC: &[u8; 8] = b"CCPLACE1";
const ZSTD_LEVEL: i32 = 19;
/// The most candidates one search looks at, to keep a vague query quick.
const MAX_CANDIDATES: usize = 400_000;

/// What kind of place an entry is. Lower sorts (and ranks) first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Kind {
    City = 0,
    Town,
    Village,
    Suburb,
    Hamlet,
    /// A named place: a shop, museum, station, office, …
    Poi,
    Street,
}

impl Kind {
    fn from_u8(v: u8) -> Kind {
        match v {
            0 => Kind::City,
            1 => Kind::Town,
            2 => Kind::Village,
            3 => Kind::Suburb,
            4 => Kind::Hamlet,
            5 => Kind::Poi,
            _ => Kind::Street,
        }
    }
}

/// One place, as found in the map data.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Record {
    pub kind: Kind,
    pub name: String,
    pub street: String,
    pub number: String,
    pub city: String,
}

/// What a region's file says about itself (stored uncompressed, so the
/// Settings window can list regions without loading them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    /// Geofabrik's id for the country, e.g. "netherlands".
    pub id: String,
    pub name: String,
    /// The country, added to towns' suggestions ("Utrecht, Netherlands").
    pub country: String,
    /// House numbers go before the street ("10 Downing Street").
    pub number_first: bool,
    /// When the map data was downloaded (YYYY-MM-DD).
    pub updated: String,
    pub entries: u64,
}

// ---------------------------------------------------------------------------
// Writing

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return Some(v);
        }
    }
    None
}

/// A column of strings as (sorted distinct strings, one code per entry).
fn dictionary<'a>(values: impl Iterator<Item = &'a str> + Clone) -> (Vec<u8>, Vec<u8>) {
    let mut distinct: Vec<&str> = values.clone().collect();
    distinct.sort_unstable();
    distinct.dedup();
    let code: HashMap<&str, u64> = distinct.iter().enumerate().map(|(i, s)| (*s, i as u64)).collect();
    let mut dict = Vec::new();
    put_varint(&mut dict, distinct.len() as u64);
    for s in &distinct {
        put_varint(&mut dict, s.len() as u64);
        dict.extend_from_slice(s.as_bytes());
    }
    let mut codes = Vec::new();
    for v in values {
        put_varint(&mut codes, code[v]);
    }
    (dict, codes)
}

/// Writes `records` (in any order, duplicates allowed) as a region file.
pub fn write(path: &Path, mut meta: Meta, mut records: Vec<Record>) -> Result<()> {
    // Kind first (it's the ranking), then town and street: the code columns
    // then hold long runs that compress to almost nothing.
    records.sort_unstable_by(|a, b| {
        (a.kind, &a.city, &a.street, &a.name, &a.number).cmp(&(b.kind, &b.city, &b.street, &b.name, &b.number))
    });
    records.dedup();
    meta.entries = records.len() as u64;

    let kinds: Vec<u8> = records.iter().map(|r| r.kind as u8).collect();
    let (name_dict, name_codes) = dictionary(records.iter().map(|r| r.name.as_str()));
    let (street_dict, street_codes) = dictionary(records.iter().map(|r| r.street.as_str()));
    let (number_dict, number_codes) = dictionary(records.iter().map(|r| r.number.as_str()));
    let (city_dict, city_codes) = dictionary(records.iter().map(|r| r.city.as_str()));

    // word -> entries (id << 1 | in name), in entry order.
    let mut words: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (id, r) in records.iter().enumerate() {
        let mut seen: HashMap<String, bool> = HashMap::new();
        for w in tokens(&r.name) {
            seen.insert(w, true);
        }
        for w in tokens(&r.street).chain(tokens(&r.city)) {
            seen.entry(w).or_insert(false);
        }
        for (w, in_name) in seen {
            words.entry(w).or_default().push((id as u64) << 1 | in_name as u64);
        }
    }
    let mut postings = Vec::new();
    let mut map = fst::MapBuilder::memory();
    for (word, ids) in &words {
        map.insert(word.as_bytes(), postings.len() as u64)?;
        put_varint(&mut postings, ids.len() as u64);
        let mut prev = 0u64;
        for &v in ids {
            // Ids only grow; the flag bit rides along in the delta.
            put_varint(&mut postings, v - prev);
            prev = v;
        }
    }
    let map = map.into_inner()?;

    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(MAGIC)?;
    let header = serde_json::to_vec(&meta)?;
    f.write_all(&(header.len() as u32).to_le_bytes())?;
    f.write_all(&header)?;
    for section in
        [&kinds, &name_dict, &name_codes, &street_dict, &street_codes, &number_dict, &number_codes, &city_dict, &city_codes, &map, &postings]
    {
        let packed = zstd::encode_all(&section[..], ZSTD_LEVEL)?;
        f.write_all(&(packed.len() as u64).to_le_bytes())?;
        f.write_all(&packed)?;
    }
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading

fn read_header(r: &mut impl Read) -> Result<Meta> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not a places file");
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut header = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut header)?;
    Ok(serde_json::from_slice(&header)?)
}

/// Just the header of a region file.
pub fn read_meta(path: &Path) -> Result<Meta> {
    read_header(&mut BufReader::new(File::open(path)?))
}

struct Column {
    strings: Vec<String>,
    codes: Vec<u32>,
}

impl Column {
    fn read(dict: &[u8], codes: &[u8], entries: usize) -> Result<Column> {
        let bad = || anyhow::anyhow!("damaged places file");
        let mut pos = 0;
        let n = get_varint(dict, &mut pos).ok_or_else(bad)? as usize;
        let mut strings = Vec::with_capacity(n.min(dict.len()));
        for _ in 0..n {
            let len = get_varint(dict, &mut pos).ok_or_else(bad)? as usize;
            let s = dict.get(pos..pos + len).ok_or_else(bad)?;
            strings.push(String::from_utf8_lossy(s).into_owned());
            pos += len;
        }
        let mut pos = 0;
        let mut out = Vec::with_capacity(entries);
        for _ in 0..entries {
            let c = get_varint(codes, &mut pos).ok_or_else(bad)? as usize;
            if c >= strings.len() {
                return Err(bad());
            }
            out.push(c as u32);
        }
        Ok(Column { strings, codes: out })
    }

    fn get(&self, i: usize) -> &str {
        &self.strings[self.codes[i] as usize]
    }
}

/// A region's places, loaded for searching.
pub struct Index {
    pub meta: Meta,
    kinds: Vec<u8>,
    names: Column,
    streets: Column,
    numbers: Column,
    cities: Column,
    words: fst::Map<Vec<u8>>,
    postings: Vec<u8>,
}

/// What was typed into the location field, ready to search for.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// Normalized words, each matched as a prefix.
    pub words: Vec<String>,
    /// A house number typed along with a street ("Damrak 70").
    pub number: Option<String>,
}

impl Query {
    pub fn parse(text: &str) -> Query {
        let mut words: Vec<String> = Vec::new();
        let mut number = None;
        // The original spelling of the number ("12a"), not the normalized one.
        for raw in text.split(|c: char| c.is_whitespace() || c == ',') {
            let is_number = raw.chars().next().is_some_and(|c| c.is_ascii_digit());
            if is_number && number.is_none() && raw.len() <= 8 {
                number = Some(raw.to_owned());
            } else {
                words.extend(tokens(raw));
            }
        }
        // Only a number: search for it as a word after all.
        if words.is_empty() {
            if let Some(n) = number.take() {
                words.extend(tokens(&n));
            }
        }
        Query { words, number }
    }
}

/// One search result: its rank (lower is better) and what to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// (0 a street for a typed house number, 1 matched by its name, 2 by
    /// its street or town; kind; position in the index)
    pub rank: (u8, Kind, usize),
    pub suggestion: Suggestion,
}

impl Index {
    pub fn load(path: &Path) -> Result<Index> {
        let mut r = BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
        let meta = read_header(&mut r)?;
        let mut sections = Vec::new();
        for _ in 0..11 {
            let mut len = [0u8; 8];
            r.read_exact(&mut len)?;
            let mut packed = vec![0u8; u64::from_le_bytes(len) as usize];
            r.read_exact(&mut packed)?;
            sections.push(zstd::decode_all(&packed[..])?);
        }
        let n = sections[0].len();
        let col = |d: usize| Column::read(&sections[d], &sections[d + 1], n);
        Ok(Index {
            names: col(1)?,
            streets: col(3)?,
            numbers: col(5)?,
            cities: col(7)?,
            words: fst::Map::new(std::mem::take(&mut sections[9]))?,
            postings: std::mem::take(&mut sections[10]),
            kinds: std::mem::take(&mut sections[0]),
            meta,
        })
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Every word starting with `prefix`, as (word, posting offset).
    fn words_with_prefix(&self, prefix: &str) -> Vec<u64> {
        let aut = fst::automaton::Str::new(prefix).starts_with();
        let mut stream = self.words.search(aut).into_stream();
        let mut out = Vec::new();
        while let Some((_, off)) = stream.next() {
            out.push(off);
        }
        out
    }

    /// How many entries the words at these offsets name, together.
    fn posting_count(&self, offsets: &[u64]) -> u64 {
        offsets.iter().map(|&o| get_varint(&self.postings, &mut (o as usize)).unwrap_or(0)).sum()
    }

    /// The entries (id << 1 | in name) under these words.
    fn postings(&self, offsets: &[u64]) -> Vec<u64> {
        let mut out = Vec::new();
        for &o in offsets {
            let mut pos = o as usize;
            let Some(n) = get_varint(&self.postings, &mut pos) else { continue };
            let mut v = 0u64;
            for _ in 0..n {
                let Some(d) = get_varint(&self.postings, &mut pos) else { break };
                v += d;
                out.push(v);
                if out.len() >= MAX_CANDIDATES {
                    return out;
                }
            }
        }
        out
    }

    fn entry_has(&self, id: usize, word: &str) -> (bool, bool) {
        let in_name = tokens(self.names.get(id)).any(|t| t.starts_with(word));
        let elsewhere = in_name
            || tokens(self.streets.get(id)).any(|t| t.starts_with(word))
            || tokens(self.cities.get(id)).any(|t| t.starts_with(word));
        (in_name, elsewhere)
    }

    /// The best `limit` entries matching every word of `q`. Entries whose
    /// own name has all the words come first, then by kind (towns before
    /// shops before streets).
    pub fn search(&self, q: &Query, limit: usize) -> Vec<Hit> {
        if q.words.is_empty() || limit == 0 {
            return Vec::new();
        }
        // Start from the word with the fewest entries.
        let mut per_word: Vec<(u64, &str, Vec<u64>)> = q
            .words
            .iter()
            .map(|w| {
                let offs = self.words_with_prefix(w);
                (self.posting_count(&offs), w.as_str(), offs)
            })
            .collect();
        per_word.sort_by_key(|(n, w, _)| (*n, std::cmp::Reverse(w.len())));
        if per_word[0].0 == 0 {
            return Vec::new();
        }
        let mut candidates = self.postings(&per_word[0].2);
        candidates.sort_unstable();
        candidates.dedup_by_key(|v| *v >> 1);

        let rest: Vec<&str> = per_word[1..].iter().map(|(_, w, _)| *w).collect();
        // With a house number typed ("Damrak 70") the user means a street:
        // streets matching by name come first.
        let want_street = q.number.is_some();
        let (mut streets, mut by_name, mut others) = (Vec::new(), Vec::new(), Vec::new());
        for v in candidates {
            let id = (v >> 1) as usize;
            if id >= self.len() {
                break;
            }
            let mut all_in_name = v & 1 == 1;
            let mut ok = true;
            for w in &rest {
                let (in_name, anywhere) = self.entry_has(id, w);
                all_in_name &= in_name;
                ok &= anywhere;
            }
            if !ok {
                continue;
            }
            if all_in_name && want_street && Kind::from_u8(self.kinds[id]) == Kind::Street {
                streets.push(id);
                if streets.len() == limit {
                    break;
                }
            } else if all_in_name {
                if by_name.len() < limit {
                    by_name.push(id);
                }
                // Candidates are in rank order: nothing later can beat these
                // (except streets, which sort last, when they're wanted).
                if by_name.len() == limit && !want_street {
                    break;
                }
            } else if others.len() < limit {
                others.push(id);
            }
        }
        let groups = [(0u8, streets), (1, by_name), (2, others)];
        let ranked = groups.into_iter().flat_map(|(g, ids)| ids.into_iter().map(move |id| (g, id)));
        ranked.take(limit).map(|(group, id)| self.hit(id, group, q)).collect()
    }

    fn hit(&self, id: usize, group: u8, q: &Query) -> Hit {
        let kind = Kind::from_u8(self.kinds[id]);
        let (name, street, number, city) = (self.names.get(id), self.streets.get(id), self.numbers.get(id), self.cities.get(id));
        let address = |street: &str, number: &str| match (street, number) {
            ("", _) => String::new(),
            (s, "") => s.to_owned(),
            (s, n) if self.meta.number_first => format!("{n} {s}"),
            (s, n) => format!("{s} {n}"),
        };
        // Without blanks or repeats ("Monaco, Monaco" for a city-state).
        let join = |parts: &[&str]| {
            let mut out: Vec<&str> = Vec::new();
            for p in parts.iter().filter(|p| !p.is_empty()) {
                if !out.contains(p) {
                    out.push(p);
                }
            }
            out.join(", ")
        };
        let suggestion = match kind {
            Kind::Street => {
                // "Damrak 70": the typed number goes with the street.
                let title = address(name, q.number.as_deref().unwrap_or(""));
                Suggestion { title, detail: join(&[city, &self.meta.country]) }
            }
            Kind::Poi => Suggestion { title: name.to_owned(), detail: join(&[&address(street, number), city]) },
            _ => Suggestion { title: name.to_owned(), detail: join(&[city, &self.meta.country]) },
        };
        Hit { rank: (group, kind, id), suggestion }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: Kind, name: &str, street: &str, number: &str, city: &str) -> Record {
        Record { kind, name: name.into(), street: street.into(), number: number.into(), city: city.into() }
    }

    fn sample() -> (tempdir::Dir, Index) {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("nl.places");
        let records = vec![
            rec(Kind::Street, "Damrak", "", "", "Amsterdam"),
            rec(Kind::Poi, "Rijksmuseum", "Museumstraat", "1", "Amsterdam"),
            rec(Kind::City, "Amsterdam", "", "", ""),
            rec(Kind::Street, "Amsterdamseweg", "", "", "Arnhem"),
            rec(Kind::Poi, "Café de Jaren", "Nieuwe Doelenstraat", "20", "Amsterdam"),
            rec(Kind::Poi, "Café de Jaren", "Nieuwe Doelenstraat", "20", "Amsterdam"),
            rec(Kind::Town, "Arnhem", "", "", ""),
        ];
        let meta = Meta { id: "netherlands".into(), name: "Netherlands".into(), country: "Netherlands".into(), ..Default::default() };
        write(&path, meta, records).unwrap();
        let idx = Index::load(&path).unwrap();
        (dir, idx)
    }

    fn labels(idx: &Index, q: &str) -> Vec<String> {
        idx.search(&Query::parse(q), 5).into_iter().map(|h| h.suggestion.label()).collect()
    }

    #[test]
    fn round_trips_and_dedups() {
        let (dir, idx) = sample();
        assert_eq!(idx.len(), 6);
        let meta = read_meta(&dir.0.join("nl.places")).unwrap();
        assert_eq!((meta.id.as_str(), meta.entries), ("netherlands", 6));
    }

    #[test]
    fn prefix_search_ranks_names_and_towns_first() {
        let (_dir, idx) = sample();
        assert_eq!(
            labels(&idx, "amster"),
            [
                "Amsterdam, Netherlands",
                "Amsterdamseweg, Arnhem, Netherlands",
                // Matched by their town only.
                "Rijksmuseum, Museumstraat 1, Amsterdam",
                "Café de Jaren, Nieuwe Doelenstraat 20, Amsterdam",
                "Damrak, Amsterdam, Netherlands",
            ]
        );
        assert_eq!(labels(&idx, "rijks"), ["Rijksmuseum, Museumstraat 1, Amsterdam"]);
        // Accents and case don't matter; every word must match.
        assert_eq!(labels(&idx, "CAFE jaren"), ["Café de Jaren, Nieuwe Doelenstraat 20, Amsterdam"]);
        assert_eq!(labels(&idx, "cafe arnhem"), Vec::<String>::new());
        // Words match from their start: "museum" finds Museumstraat, not
        // "Rijksmuseum" itself, and "jaren" alone finds nothing in Arnhem.
        assert_eq!(labels(&idx, "museum amsterdam"), ["Rijksmuseum, Museumstraat 1, Amsterdam"]);
        assert_eq!(labels(&idx, "useum"), Vec::<String>::new());
    }

    #[test]
    fn a_typed_house_number_goes_with_the_street() {
        let (_dir, idx) = sample();
        assert_eq!(labels(&idx, "damrak 70"), ["Damrak 70, Amsterdam, Netherlands"]);
        assert_eq!(Query::parse("12a Main St").number.as_deref(), Some("12a"));
        assert_eq!(Query::parse("1940").words, ["1940"]);
    }

    #[test]
    fn varints_round_trip() {
        let mut buf = Vec::new();
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            put_varint(&mut buf, v);
        }
        let mut pos = 0;
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            assert_eq!(get_varint(&buf, &mut pos), Some(v));
        }
        assert_eq!(get_varint(&buf, &mut pos), None);
    }

    /// A temporary directory, removed when dropped.
    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                let p = std::env::temp_dir().join(format!("cc-places-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
