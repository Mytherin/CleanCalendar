//! Reads an OpenStreetMap extract (`.osm.pbf`) into place records: towns
//! and villages, named places (shops, museums, stations, …) and streets.
//!
//! Streets and buildings are OSM *ways*, which only reference their nodes,
//! so the file is read in three parallel passes over its blocks:
//! 1. everything: towns and named nodes, plus which node each interesting
//!    way starts at (one node is close enough to say where a way is);
//! 2. the blocks with nodes: the coordinates of those starting nodes;
//! 3. the blocks with ways: the ways themselves, now with a location.
//!
//! Each street and place without an address of its own is then put in the
//! town nearest to it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{bail, Result};
use osmpbf::{BlobDecode, BlobReader, ByteOffset, Element};
use rayon::prelude::*;

use super::index::{Kind, Record};

/// How far along the reading is, for the Settings window.
pub struct Progress<'a> {
    /// Blocks read so far, out of `total` (both across all three passes).
    pub done: &'a AtomicUsize,
    pub total: &'a AtomicUsize,
    pub cancel: &'a AtomicBool,
}

/// The tags this reader cares about.
#[derive(Default)]
struct Tags<'a> {
    name: Option<&'a str>,
    place: Option<&'a str>,
    highway: Option<&'a str>,
    street: Option<&'a str>,
    number: Option<&'a str>,
    city: Option<&'a str>,
    /// Tagged as something people go to (a shop, museum, station, …).
    poi: bool,
}

/// Amenities that are rarely a destination worth suggesting.
const DULL_AMENITIES: &[&str] = &[
    "bench", "waste_basket", "bicycle_parking", "parking", "parking_space", "parking_entrance", "vending_machine",
    "post_box", "recycling", "toilets", "telephone", "atm", "drinking_water", "shelter", "waste_disposal",
    "charging_station", "motorcycle_parking", "hunting_stand", "grit_bin", "clock",
];
const STREETS: &[&str] = &[
    "motorway", "trunk", "primary", "secondary", "tertiary", "unclassified", "residential", "living_street",
    "pedestrian", "road",
];

impl<'a> Tags<'a> {
    fn read(tags: impl Iterator<Item = (&'a str, &'a str)>) -> Tags<'a> {
        let mut t = Tags::default();
        for (k, v) in tags {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k {
                "name" => t.name = Some(v),
                "place" => t.place = Some(v),
                "highway" => t.highway = Some(v),
                "addr:street" => t.street = Some(v),
                "addr:housenumber" => t.number = Some(v),
                "addr:city" => t.city = Some(v),
                "amenity" => t.poi |= !DULL_AMENITIES.contains(&v),
                "shop" | "tourism" | "leisure" | "office" | "craft" | "historic" | "healthcare" | "club" | "building" => {
                    t.poi = true
                }
                "railway" => t.poi |= matches!(v, "station" | "halt"),
                "public_transport" => t.poi |= v == "station",
                "aeroway" => t.poi |= matches!(v, "aerodrome" | "terminal"),
                _ => {}
            }
        }
        t
    }

    fn place_kind(&self) -> Option<Kind> {
        Some(match self.place? {
            "city" => Kind::City,
            "town" => Kind::Town,
            "village" => Kind::Village,
            "suburb" | "quarter" | "neighbourhood" | "borough" => Kind::Suburb,
            "hamlet" | "isolated_dwelling" => Kind::Hamlet,
            _ => return None,
        })
    }

    /// The record this node or way becomes (its town still to be found
    /// unless it has an address), if any.
    fn record(&self, is_way: bool) -> Option<Record> {
        let name = self.name?;
        let kind = if let Some(k) = self.place_kind().filter(|_| !is_way) {
            k
        } else if self.poi {
            Kind::Poi
        } else if is_way && self.highway.is_some_and(|h| STREETS.contains(&h)) {
            Kind::Street
        } else {
            return None;
        };
        let poi = kind == Kind::Poi;
        Some(Record {
            kind,
            name: name.to_owned(),
            street: if poi { self.street.unwrap_or_default().to_owned() } else { String::new() },
            number: if poi && self.street.is_some() { self.number.unwrap_or_default().to_owned() } else { String::new() },
            city: if poi { self.city.unwrap_or_default().to_owned() } else { String::new() },
        })
    }
}

/// A record with where it is (degrees, as f32: ~1 m is plenty).
type Located = (Record, f32, f32);

/// What one block of the file holds, from pass 1.
#[derive(Default)]
struct Pass1 {
    nodes: bool,
    ways: bool,
    found: Vec<Located>,
    /// The first node of each interesting way.
    want: Vec<i64>,
}

fn blocks(path: &Path) -> Result<Vec<ByteOffset>> {
    // Only the headers are read here; it's quick.
    let mut r = BlobReader::seekable_from_path(path)?;
    let mut out = Vec::new();
    while let Some(next) = r.next_header_skip_blob() {
        let (header, offset) = next?;
        if header.blob_type() == osmpbf::BlobType::OsmData {
            out.extend(offset);
        }
    }
    Ok(out)
}

/// Runs `f` on the decoded block at each offset, in parallel.
fn each_block<T: Send>(
    path: &Path,
    offsets: &[ByteOffset],
    progress: &Progress,
    f: impl Fn(&osmpbf::PrimitiveBlock) -> T + Sync,
) -> Result<Vec<T>> {
    offsets
        .par_iter()
        .map_init(
            || BlobReader::seekable_from_path(path),
            |reader, &off| {
                if progress.cancel.load(Ordering::Relaxed) {
                    bail!("cancelled");
                }
                let reader = reader.as_mut().map_err(|e| anyhow::anyhow!("{e}"))?;
                let blob = reader.blob_from_offset(off)?;
                let out = match blob.decode()? {
                    BlobDecode::OsmData(block) => f(&block),
                    _ => bail!("unexpected block"),
                };
                progress.done.fetch_add(1, Ordering::Relaxed);
                Ok(out)
            },
        )
        .collect()
}

/// Reads the extract at `path` into records, each with its town.
pub fn read(path: &Path, progress: &Progress) -> Result<Vec<Record>> {
    let offsets = blocks(path)?;
    // Passes 2 and 3 together read each block about once more.
    progress.total.store(offsets.len() * 2, Ordering::Relaxed);

    // Pass 1.
    let pass1 = each_block(path, &offsets, progress, |block| {
        let mut out = Pass1::default();
        for el in block.elements() {
            let (tags, lat, lon) = match &el {
                Element::Node(n) => (Tags::read(n.tags()), n.lat(), n.lon()),
                Element::DenseNode(n) => (Tags::read(n.tags()), n.lat(), n.lon()),
                Element::Way(w) => {
                    out.ways = true;
                    let tags = Tags::read(w.tags());
                    if tags.record(true).is_some() {
                        out.want.extend(w.refs().next());
                    }
                    continue;
                }
                Element::Relation(_) => continue,
            };
            out.nodes = true;
            if let Some(r) = tags.record(false) {
                out.found.push((r, lat as f32, lon as f32));
            }
        }
        out
    })?;
    let node_blocks: Vec<ByteOffset> = offsets.iter().zip(&pass1).filter(|(_, p)| p.nodes).map(|(o, _)| *o).collect();
    let way_blocks: Vec<ByteOffset> = offsets.iter().zip(&pass1).filter(|(_, p)| p.ways).map(|(o, _)| *o).collect();
    progress.total.store(offsets.len() + node_blocks.len() + way_blocks.len(), Ordering::Relaxed);
    let mut want: Vec<i64> = pass1.iter().flat_map(|p| p.want.iter().copied()).collect();
    want.sort_unstable();
    want.dedup();
    let mut found: Vec<Located> = pass1.into_iter().flat_map(|p| p.found).collect();

    // Pass 2: where the wanted nodes are.
    let located = each_block(path, &node_blocks, progress, |block| {
        let mut out = Vec::new();
        let mut take = |id: i64, lat: f64, lon: f64| {
            if let Ok(i) = want.binary_search(&id) {
                out.push((i, lat as f32, lon as f32));
            }
        };
        for el in block.elements() {
            match el {
                Element::Node(n) => take(n.id(), n.lat(), n.lon()),
                Element::DenseNode(n) => take(n.id(), n.lat(), n.lon()),
                _ => {}
            }
        }
        out
    })?;
    let mut coords = vec![(f32::NAN, f32::NAN); want.len()];
    for (i, lat, lon) in located.into_iter().flatten() {
        coords[i] = (lat, lon);
    }

    // Pass 3: the ways.
    let ways = each_block(path, &way_blocks, progress, |block| {
        let mut out = Vec::new();
        for el in block.elements() {
            let Element::Way(w) = el else { continue };
            let Some(rec) = Tags::read(w.tags()).record(true) else { continue };
            let Some(first) = w.refs().next() else { continue };
            if let Ok(i) = want.binary_search(&first) {
                let (lat, lon) = coords[i];
                if !lat.is_nan() {
                    out.push((rec, lat, lon));
                }
            }
        }
        out
    })?;
    found.extend(ways.into_iter().flatten());
    Ok(assign_towns(found))
}

/// How far a town of this kind reaches, in km, for finding the town a place
/// is in. A place belongs to the town that is nearest relative to its size.
fn reach_km(kind: Kind) -> Option<f32> {
    match kind {
        Kind::City => Some(12.0),
        Kind::Town => Some(5.0),
        Kind::Village => Some(2.0),
        _ => None,
    }
}

/// Grid cells of 0.1°; a city reaches about two cells.
const CELL: f32 = 0.1;

fn cell(lat: f32, lon: f32) -> (i32, i32) {
    ((lat / CELL).floor() as i32, (lon / CELL).floor() as i32)
}

/// Fills in each record's town (if it has none from its address), then
/// drops the duplicates a street split into many ways leaves behind.
pub fn assign_towns(found: Vec<Located>) -> Vec<Record> {
    let towns: Vec<(String, f32, f32, f32)> = found
        .iter()
        .filter_map(|(r, lat, lon)| reach_km(r.kind).map(|km| (r.name.clone(), *lat, *lon, km)))
        .collect();
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, (_, lat, lon, _)) in towns.iter().enumerate() {
        grid.entry(cell(*lat, *lon)).or_default().push(i);
    }
    let town_of = |name: &str, lat: f32, lon: f32| -> Option<String> {
        let (cy, cx) = cell(lat, lon);
        let mut best: Option<(f32, usize)> = None;
        for dy in -2..=2 {
            for dx in -2..=2 {
                for &i in grid.get(&(cy + dy, cx + dx)).into_iter().flatten() {
                    let (tname, tlat, tlon, km) = (&towns[i].0, towns[i].1, towns[i].2, towns[i].3);
                    if tname == name {
                        continue;
                    }
                    // Equirectangular distance is plenty at this scale.
                    let x = (lon - tlon) * (lat.to_radians().cos()) * 111.3;
                    let y = (lat - tlat) * 111.3;
                    let score = (x * x + y * y).sqrt() / km;
                    if score <= 2.0 && best.is_none_or(|(b, _)| score < b) {
                        best = Some((score, i));
                    }
                }
            }
        }
        best.map(|(_, i)| towns[i].0.clone())
    };
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(found.len());
    for (mut r, lat, lon) in found {
        let is_town = matches!(r.kind, Kind::City | Kind::Town | Kind::Village);
        if r.city.is_empty() && !is_town {
            r.city = town_of(&r.name, lat, lon).unwrap_or_default();
        }
        if seen.insert(r.clone()) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_decide_what_a_feature_becomes() {
        let rec = |tags: &[(&'static str, &'static str)], way: bool| Tags::read(tags.iter().copied()).record(way);
        let museum = rec(&[("name", "Rijksmuseum"), ("tourism", "museum"), ("addr:street", "Museumstraat"), ("addr:housenumber", "1")], true).unwrap();
        assert_eq!((museum.kind, museum.street.as_str(), museum.number.as_str()), (Kind::Poi, "Museumstraat", "1"));
        assert_eq!(rec(&[("name", "Damrak"), ("highway", "primary")], true).unwrap().kind, Kind::Street);
        assert_eq!(rec(&[("name", "Utrecht"), ("place", "city")], false).unwrap().kind, Kind::City);
        // Nameless, dull, or not a street people go to.
        assert!(rec(&[("tourism", "museum")], false).is_none());
        assert!(rec(&[("name", "P1"), ("amenity", "parking")], false).is_none());
        assert!(rec(&[("name", "Fietspad"), ("highway", "cycleway")], true).is_none());
    }

    #[test]
    fn places_get_the_town_they_are_in() {
        let r = |kind, name: &str| Record { kind, name: name.into(), street: String::new(), number: String::new(), city: String::new() };
        let found = vec![
            (r(Kind::City, "Amsterdam"), 52.373, 4.892),
            (r(Kind::Village, "Ouderkerk"), 52.295, 4.920),
            // Two pieces of the same street.
            (r(Kind::Street, "Damrak"), 52.376, 4.896),
            (r(Kind::Street, "Damrak"), 52.375, 4.895),
            // In the village, though the city is not far.
            (r(Kind::Street, "Amstelzijde"), 52.30, 4.93),
            (r(Kind::Street, "Nowhere Road"), 10.0, 10.0),
        ];
        let out: Vec<(String, String)> = assign_towns(found).into_iter().map(|r| (r.name, r.city)).collect();
        assert_eq!(
            out,
            [
                ("Amsterdam".into(), "".into()),
                // Towns and villages are their own town.
                ("Ouderkerk".into(), "".into()),
                ("Damrak".into(), "Amsterdam".into()),
                ("Amstelzijde".into(), "Ouderkerk".into()),
                ("Nowhere Road".into(), "".into()),
            ]
        );
    }
}
