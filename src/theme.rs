//! Apple Calendar–like colors and fonts, for both light and dark mode.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId};

pub const RED: Color32 = Color32::from_rgb(0xFF, 0x3B, 0x30);
pub const ACCENT: Color32 = Color32::from_rgb(0x00, 0x7A, 0xFF);

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub sidebar: Color32,
    pub toolbar: Color32,
    pub grid: Color32,
    pub grid_strong: Color32,
    pub weekend: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub tertiary: Color32,
    pub selection: Color32,
}

impl Palette {
    pub fn get(ctx: &egui::Context) -> Self {
        if ctx.style().visuals.dark_mode {
            Palette {
                dark: true,
                bg: Color32::from_rgb(0x1E, 0x1E, 0x1E),
                sidebar: Color32::from_rgb(0x28, 0x28, 0x2A),
                toolbar: Color32::from_rgb(0x2C, 0x2C, 0x2E),
                grid: Color32::from_rgb(0x38, 0x38, 0x3A),
                grid_strong: Color32::from_rgb(0x48, 0x48, 0x4A),
                weekend: Color32::from_rgb(0x23, 0x23, 0x24),
                text: Color32::from_rgb(0xF2, 0xF2, 0xF7),
                secondary: Color32::from_rgb(0x98, 0x98, 0x9F),
                tertiary: Color32::from_rgb(0x63, 0x63, 0x66),
                selection: Color32::from_rgb(0x3A, 0x3A, 0x3C),
            }
        } else {
            Palette {
                dark: false,
                bg: Color32::WHITE,
                sidebar: Color32::from_rgb(0xF2, 0xF1, 0xF6),
                toolbar: Color32::from_rgb(0xF6, 0xF6, 0xF6),
                grid: Color32::from_rgb(0xE6, 0xE6, 0xE6),
                grid_strong: Color32::from_rgb(0xD1, 0xD1, 0xD6),
                weekend: Color32::from_rgb(0xFA, 0xFA, 0xFA),
                text: Color32::from_rgb(0x1D, 0x1D, 0x1F),
                secondary: Color32::from_rgb(0x8E, 0x8E, 0x93),
                tertiary: Color32::from_rgb(0xC7, 0xC7, 0xCC),
                selection: Color32::from_rgb(0xE8, 0xE8, 0xED),
            }
        }
    }

    /// Translucent fill used behind timed events.
    pub fn event_fill(&self, c: Color32, selected: bool) -> Color32 {
        if selected {
            return c;
        }
        let a = if self.dark { 0.30 } else { 0.18 };
        blend(self.bg, c, a)
    }

    /// Text color for an event drawn on its translucent fill.
    pub fn event_text(&self, c: Color32, selected: bool) -> Color32 {
        if selected {
            return Color32::WHITE;
        }
        if self.dark {
            blend(c, Color32::WHITE, 0.45)
        } else {
            blend(c, Color32::BLACK, 0.45)
        }
    }
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, bold())
}

/// Use the platform UI font when available (Segoe UI on Windows, SF on macOS),
/// falling back to egui's built-in font.
pub fn install_fonts(ctx: &egui::Context) {
    let regular = [
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    let bold_paths = [
        "C:\\Windows\\Fonts\\seguisb.ttf",
        "C:\\Windows\\Fonts\\segoeuib.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ];
    let load = |paths: &[&str]| paths.iter().find_map(|p| std::fs::read(p).ok());

    let mut fonts = FontDefinitions::default();
    if let Some(bytes) = load(&regular) {
        fonts.font_data.insert("ui".into(), FontData::from_owned(bytes));
        fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "ui".into());
    }
    let mut bold_family = fonts.families[&FontFamily::Proportional].clone();
    if let Some(bytes) = load(&bold_paths) {
        fonts.font_data.insert("ui-bold".into(), FontData::from_owned(bytes));
        bold_family.insert(0, "ui-bold".into());
    }
    fonts.families.insert(bold(), bold_family);
    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context) {
    let p = Palette::get(ctx);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.visuals.panel_fill = p.bg;
        s.visuals.window_fill = p.bg;
        s.visuals.window_rounding = egui::Rounding::same(12.0);
        s.visuals.menu_rounding = egui::Rounding::same(8.0);
        s.visuals.selection.bg_fill = ACCENT;
        s.visuals.hyperlink_color = ACCENT;
        for w in [
            &mut s.visuals.widgets.inactive,
            &mut s.visuals.widgets.hovered,
            &mut s.visuals.widgets.active,
            &mut s.visuals.widgets.open,
        ] {
            w.rounding = egui::Rounding::same(6.0);
        }
        s.visuals.widgets.noninteractive.bg_stroke.color = p.grid;
        s.text_styles.insert(egui::TextStyle::Body, font(13.5));
        s.text_styles.insert(egui::TextStyle::Button, font(13.5));
        s.text_styles.insert(egui::TextStyle::Small, font(11.0));
        s.text_styles.insert(egui::TextStyle::Heading, bold_font(20.0));
    });
}
