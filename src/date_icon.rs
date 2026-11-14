//! Renders the app icon for a given day: a white rounded tile with the
//! weekday in red over a large day number, like the macOS Calendar icon.
//!
//! Pure software rasterisation (rounded-rect SDF + `ab_glyph` text) into a
//! straight-alpha RGBA buffer, so it can feed window, taskbar and tray icons.

use std::sync::OnceLock;

use ab_glyph::{Font, FontArc, Glyph, PxScale, ScaleFont};
use chrono::{Datelike, NaiveDate};

const RED: [f32; 3] = [255.0, 59.0, 48.0];
const INK: [f32; 3] = [28.0, 28.0, 30.0];
const TILE: [f32; 3] = [255.0, 255.0, 255.0];

/// Sizes at or below this use the compact "red band" layout: weekday text
/// would be unreadable, so the weekday becomes a red strip across the top.
const COMPACT_MAX: u32 = 40;

struct Fonts {
    regular: FontArc,
    bold: FontArc,
}

fn fonts() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(|| {
        let load = |paths: &[&str]| {
            paths.iter().find_map(|p| std::fs::read(p).ok()).and_then(|b| FontArc::try_from_vec(b).ok())
        };
        // egui's bundled font is the last resort, so this never fails.
        let fallback = || {
            let defs = eframe::egui::FontDefinitions::default();
            let data = defs.font_data.get("Ubuntu-Light").or_else(|| defs.font_data.values().next());
            let data = data.expect("egui ships fonts").font.clone();
            FontArc::try_from_vec(data.into_owned()).expect("egui font parses")
        };
        let regular = load(&[
            "C:\\Windows\\Fonts\\segoeui.ttf",
            "/System/Library/Fonts/SFNS.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ])
        .unwrap_or_else(fallback);
        let bold = load(&[
            "C:\\Windows\\Fonts\\seguisb.ttf",
            "C:\\Windows\\Fonts\\segoeuib.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        ])
        .unwrap_or_else(|| regular.clone());
        Fonts { regular, bold }
    })
}

/// Straight-alpha RGBA canvas with float channels for clean blending.
struct Canvas {
    size: u32,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(size: u32) -> Self {
        Canvas { size, px: vec![[0.0; 4]; (size * size) as usize] }
    }

    /// Composites `color` at coverage `a` over pixel (x, y).
    fn blend(&mut self, x: u32, y: u32, color: [f32; 3], a: f32) {
        if a <= 0.0 || x >= self.size || y >= self.size {
            return;
        }
        let a = a.min(1.0);
        let p = &mut self.px[(y * self.size + x) as usize];
        let out_a = a + p[3] * (1.0 - a);
        if out_a > 0.0 {
            for c in 0..3 {
                p[c] = (color[c] * a + p[c] * p[3] * (1.0 - a)) / out_a;
            }
        }
        p[3] = out_a;
    }

    fn into_rgba(self) -> Vec<u8> {
        self.px
            .into_iter()
            .flat_map(|[r, g, b, a]| [r.round() as u8, g.round() as u8, b.round() as u8, (a * 255.0).round() as u8])
            .collect()
    }
}

/// Axis-aligned rounded rectangle, evaluated as a signed distance field.
#[derive(Clone, Copy)]
struct RoundRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: f32,
}

impl RoundRect {
    fn sdf(&self, x: f32, y: f32) -> f32 {
        let (cx, cy) = ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0);
        let (hx, hy) = ((self.x1 - self.x0) / 2.0 - self.r, (self.y1 - self.y0) / 2.0 - self.r);
        let qx = (x - cx).abs() - hx;
        let qy = (y - cy).abs() - hy;
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        outside + qx.max(qy).min(0.0) - self.r
    }

    /// Anti-aliased coverage of the pixel whose centre is (x, y).
    fn coverage(&self, x: f32, y: f32) -> f32 {
        (0.5 - self.sdf(x, y)).clamp(0.0, 1.0)
    }
}

/// Lays out `text` at `scale`; returns glyphs positioned at the origin.
fn layout(font: &FontArc, text: &str, scale: PxScale) -> Vec<Glyph> {
    let sf = font.as_scaled(scale);
    let mut x = 0.0;
    let mut prev = None;
    let mut glyphs = Vec::new();
    for ch in text.chars() {
        let id = sf.glyph_id(ch);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        glyphs.push(id.with_scale_and_position(scale, ab_glyph::point(x, 0.0)));
        x += sf.h_advance(id);
        prev = Some(id);
    }
    glyphs
}

/// Ink bounds (x0, y0, x1, y1) of laid-out glyphs.
fn ink_bounds(font: &FontArc, glyphs: &[Glyph]) -> Option<(f32, f32, f32, f32)> {
    glyphs.iter().filter_map(|g| font.outline_glyph(g.clone())).map(|o| o.px_bounds()).fold(None, |acc, b| {
        Some(match acc {
            None => (b.min.x, b.min.y, b.max.x, b.max.y),
            Some((x0, y0, x1, y1)) => (x0.min(b.min.x), y0.min(b.min.y), x1.max(b.max.x), y1.max(b.max.y)),
        })
    })
}

/// Draws `text` so its ink is `height` px tall (shrunk to fit `max_width`)
/// and centred on (cx, cy). Coverage is clipped to `clip`.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    canvas: &mut Canvas,
    font: &FontArc,
    text: &str,
    height: f32,
    max_width: f32,
    cx: f32,
    cy: f32,
    color: [f32; 3],
    clip: &RoundRect,
) {
    // Measure at a reference size, then scale linearly to the target.
    let probe = PxScale::from(100.0);
    let Some((x0, y0, x1, y1)) = ink_bounds(font, &layout(font, text, probe)) else { return };
    let factor = (height / (y1 - y0)).min(max_width / (x1 - x0));
    let scale = PxScale::from(100.0 * factor);
    let glyphs = layout(font, text, scale);
    let Some((x0, y0, x1, y1)) = ink_bounds(font, &glyphs) else { return };
    // Snap the offset to whole pixels so stems stay crisp at small sizes.
    let dx = (cx - (x0 + x1) / 2.0).round();
    let dy = (cy - (y0 + y1) / 2.0).round();
    for g in glyphs {
        let Some(outline) = font.outline_glyph(g) else { continue };
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let x = b.min.x + gx as f32 + dx;
            let y = b.min.y + gy as f32 + dy;
            if x < 0.0 || y < 0.0 {
                return;
            }
            let a = cov * clip.coverage(x + 0.5, y + 0.5);
            canvas.blend(x as u32, y as u32, color, a);
        });
    }
}

fn weekday_label(date: NaiveDate) -> &'static str {
    ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"][date.weekday().num_days_from_monday() as usize]
}

/// Renders the icon for `date` as `size`×`size` straight-alpha RGBA bytes.
pub fn render(date: NaiveDate, size: u32) -> Vec<u8> {
    let s = size as f32;
    let fonts = fonts();
    let mut canvas = Canvas::new(size);
    let compact = size <= COMPACT_MAX;

    // Small icons use the full square; large ones leave room for a shadow,
    // matching the inset of standard Windows / macOS app icons.
    let inset = if compact { 0.0 } else { (s * 0.07).round() };
    let tile = RoundRect { x0: inset, y0: inset, x1: s - inset, y1: s - inset, r: (s - 2.0 * inset) * 0.2 };
    let w = tile.x1 - tile.x0;

    // Soft drop shadow, then the tile with a hairline edge so it holds up on
    // white backgrounds.
    if !compact {
        let blur = s * 0.035;
        let shadow = RoundRect { y0: tile.y0 + s * 0.02, y1: tile.y1 + s * 0.02, ..tile };
        for y in 0..size {
            for x in 0..size {
                let d = shadow.sdf(x as f32 + 0.5, y as f32 + 0.5);
                let a = 0.28 * (1.0 - (d / blur + 0.5).clamp(0.0, 1.0));
                canvas.blend(x, y, [0.0; 3], a);
            }
        }
    }
    let edge = (s / 64.0).max(1.0);
    let inner = RoundRect { x0: tile.x0 + edge, y0: tile.y0 + edge, x1: tile.x1 - edge, y1: tile.y1 - edge, r: tile.r - edge };
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            canvas.blend(x, y, [190.0, 190.0, 196.0], tile.coverage(px, py));
            canvas.blend(x, y, TILE, inner.coverage(px, py));
        }
    }

    let day = date.day().to_string();
    let cx = s / 2.0;
    if compact {
        // Red band for the weekday, big bold number below it.
        let band_h = (w * 0.3).round();
        for y in 0..size {
            for x in 0..size {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let a = tile.coverage(px, py) * (band_h - (py - 0.5)).clamp(0.0, 1.0);
                canvas.blend(x, y, RED, a);
            }
        }
        let body_top = tile.y0 + band_h;
        let body_h = tile.y1 - body_top;
        draw_text(&mut canvas, &fonts.bold, &day, body_h * 0.62, w * 0.86, cx, body_top + body_h * 0.5, INK, &tile);
    } else {
        draw_text(&mut canvas, &fonts.bold, weekday_label(date), w * 0.13, w * 0.7, cx, tile.y0 + w * 0.2, RED, &tile);
        draw_text(&mut canvas, &fonts.regular, &day, w * 0.46, w * 0.8, cx, tile.y0 + w * 0.6, INK, &tile);
    }
    canvas.into_rgba()
}

/// The icon for `date` as egui icon data, for the viewport.
pub fn icon_data(date: NaiveDate, size: u32) -> eframe::egui::IconData {
    eframe::egui::IconData { rgba: render(date, size), width: size, height: size }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_size_with_opaque_centre_and_transparent_corner() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        for size in [16, 20, 24, 32, 48, 64, 256] {
            let rgba = render(date, size);
            assert_eq!(rgba.len(), (size * size * 4) as usize);
            let alpha = |x: u32, y: u32| rgba[((y * size + x) * 4 + 3) as usize];
            assert_eq!(alpha(0, 0), 0, "corner should be transparent at {size}px");
            assert_eq!(alpha(size / 2, size / 2), 255, "centre should be opaque at {size}px");
        }
    }

    #[test]
    fn draws_red_weekday_and_dark_digits() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        for size in [16, 128] {
            let rgba = render(date, size);
            let px: Vec<&[u8]> = rgba.chunks(4).collect();
            let red = px.iter().filter(|p| p[0] > 200 && p[1] < 120 && p[3] > 200).count();
            let dark = px.iter().filter(|p| p[0] < 90 && p[1] < 90 && p[2] < 90 && p[3] > 200).count();
            assert!(red > 0, "no red weekday pixels at {size}px");
            assert!(dark > 0, "no dark digit pixels at {size}px");
        }
    }

    /// Writes sample icons as 32-bit BMP files for eyeballing:
    /// `cargo test date_icon::tests::dump -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump() {
        let dir = std::env::temp_dir().join("clean-calendar-icons");
        std::fs::create_dir_all(&dir).unwrap();
        for (d, size) in [(14, 16), (14, 24), (14, 32), (28, 48), (14, 64), (7, 256)] {
            let date = NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
            let rgba = render(date, size);
            let data_len = size * size * 4;
            let mut out = Vec::new();
            out.extend(b"BM");
            out.extend((54 + data_len).to_le_bytes());
            out.extend([0u8; 4]);
            out.extend(54u32.to_le_bytes());
            out.extend(40u32.to_le_bytes());
            out.extend((size as i32).to_le_bytes());
            out.extend((-(size as i32)).to_le_bytes()); // top-down
            out.extend(1u16.to_le_bytes());
            out.extend(32u16.to_le_bytes());
            out.extend([0u8; 24]);
            // Composite over light grey so transparency is visible in viewers.
            let over = |c: u8, a: u8| ((c as u32 * a as u32 + 230 * (255 - a as u32)) / 255) as u8;
            out.extend(rgba.chunks(4).flat_map(|p| [over(p[2], p[3]), over(p[1], p[3]), over(p[0], p[3]), 255]));
            std::fs::write(dir.join(format!("icon_{size}.bmp")), out).unwrap();
        }
        println!("{}", dir.display());
    }

    /// Regenerates `assets/CleanCalendar.ico`, the icon `build.rs` embeds in
    /// the Windows executable (what Explorer, shortcuts and the Start menu
    /// show before the app runs and draws today's date):
    /// `cargo test date_icon::tests::write_exe_icon -- --ignored`
    #[test]
    #[ignore]
    fn write_exe_icon() {
        // A fixed, two-digit day reads well at every size.
        let date = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        let sizes = [16u32, 20, 24, 32, 40, 48, 64, 256];
        let images: Vec<Vec<u8>> = sizes.iter().map(|&size| ico_image(&render(date, size), size)).collect();

        let mut ico = Vec::new();
        ico.extend([0u16, 1, sizes.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
        let mut offset = 6 + 16 * sizes.len() as u32;
        for (&size, image) in sizes.iter().zip(&images) {
            let dim = if size >= 256 { 0 } else { size as u8 };
            ico.extend([dim, dim, 0, 0]);
            ico.extend(1u16.to_le_bytes()); // planes
            ico.extend(32u16.to_le_bytes()); // bits per pixel
            ico.extend((image.len() as u32).to_le_bytes());
            ico.extend(offset.to_le_bytes());
            offset += image.len() as u32;
        }
        images.iter().for_each(|image| ico.extend(image));

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("CleanCalendar.ico");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, ico).unwrap();
        println!("{}", path.display());
    }

    /// One icon image: PNG at 256px (keeps the file small), a classic 32-bit
    /// DIB below that, which every Windows shell component reads.
    fn ico_image(rgba: &[u8], size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        if size >= 256 {
            let mut enc = png::Encoder::new(&mut out, size, size);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Best);
            enc.write_header().unwrap().write_image_data(rgba).unwrap();
            return out;
        }
        out.extend(40u32.to_le_bytes());
        out.extend((size as i32).to_le_bytes());
        out.extend((2 * size as i32).to_le_bytes()); // XOR image + AND mask
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend([0u8; 24]);
        // Bottom-up BGRA rows, then an all-zero AND mask (alpha does the work)
        // with rows padded to 32 bits.
        for row in rgba.chunks_exact(size as usize * 4).rev() {
            out.extend(row.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]));
        }
        out.extend(std::iter::repeat(0u8).take(size.div_ceil(32) as usize * 4 * size as usize));
        out
    }
}
