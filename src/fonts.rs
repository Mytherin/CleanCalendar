//! Finds the platform's UI font files: Segoe UI on Windows, San Francisco
//! (with Helvetica Neue for bold) on macOS, DejaVu Sans on Linux.
//!
//! Shared by the egui theme and the date-icon renderer so both draw with the
//! same faces. Callers fall back to egui's bundled font when nothing's found.

use std::sync::OnceLock;

/// A font file's bytes and which face in it to use (non-zero only for
/// collections such as `.ttc`).
#[derive(Clone)]
pub struct FontFile {
    pub bytes: Vec<u8>,
    pub index: u32,
}

const REGULAR: &[&str] = &[
    // Windows
    "C:\\Windows\\Fonts\\segoeui.ttf",
    // macOS: SF is a variable font whose default instance is Regular.
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/HelveticaNeue.ttc",
    // Linux (Debian/Ubuntu, Fedora, Arch)
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

const BOLD: &[&str] = &[
    // Windows: Semibold, else Bold.
    "C:\\Windows\\Fonts\\seguisb.ttf",
    "C:\\Windows\\Fonts\\segoeuib.ttf",
    // macOS: SF ships no static bold, and egui can't pick a variable font's
    // weight, so use Helvetica Neue's Bold face (picked out of the collection).
    "/System/Library/Fonts/HelveticaNeue.ttc",
    // Linux
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
];

pub fn regular() -> Option<&'static FontFile> {
    static F: OnceLock<Option<FontFile>> = OnceLock::new();
    F.get_or_init(|| find(REGULAR, false)).as_ref()
}

pub fn bold() -> Option<&'static FontFile> {
    static F: OnceLock<Option<FontFile>> = OnceLock::new();
    F.get_or_init(|| find(BOLD, true)).as_ref()
}

fn find(paths: &[&str], bold: bool) -> Option<FontFile> {
    paths.iter().find_map(|p| {
        let bytes = std::fs::read(p).ok()?;
        let index = pick_face(&bytes, bold)?;
        Some(FontFile { bytes, index })
    })
}

/// The face to use from a font file. A single-face file is taken as is (the
/// path lists say what it is); in a collection, the first upright,
/// normal-width face of the wanted weight.
fn pick_face(bytes: &[u8], bold: bool) -> Option<u32> {
    let Some(count) = ttf_parser::fonts_in_collection(bytes) else {
        return ttf_parser::Face::parse(bytes, 0).is_ok().then_some(0);
    };
    (0..count).find(|&i| {
        ttf_parser::Face::parse(bytes, i).is_ok_and(|f| {
            let weight = f.weight().to_number();
            !f.is_italic()
                && !f.is_oblique()
                && f.width() == ttf_parser::Width::Normal
                && if bold { (600..=700).contains(&weight) } else { weight == 400 }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_not_a_font() {
        assert_eq!(pick_face(b"not a font", false), None);
        assert!(find(&["/no/such/font.ttf"], false).is_none());
    }

    #[test]
    fn every_platform_finds_its_fonts_when_present() {
        // Whatever is found must parse as the face we said to use.
        for f in [regular(), bold()].into_iter().flatten() {
            assert!(ttf_parser::Face::parse(&f.bytes, f.index).is_ok());
        }
    }
}
