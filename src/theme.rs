//! Apple Calendar–like colors and fonts, for both light and dark mode.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId};

/// Today markers and the year view's month names (macOS dark-mode red).
pub const RED: Color32 = Color32::from_rgb(0xEB, 0x53, 0x4E);
pub const ACCENT: Color32 = Color32::from_rgb(0x00, 0x7A, 0xFF);

/// Colors measured from macOS Calendar screenshots (dark mode), with a light
/// counterpart.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Window and calendar background.
    pub bg: Color32,
    /// The inset sidebar panel.
    pub sidebar: Color32,
    pub toolbar: Color32,
    /// Hour lines, column lines, cell borders.
    pub grid: Color32,
    /// The line under the all-day lane.
    pub grid_strong: Color32,
    /// Hours outside the working day in the day/week grid.
    pub off_hours: Color32,
    /// Weekend cells in the month view.
    pub weekend: Color32,
    pub text: Color32,
    /// Hour labels, weekend headers, event times.
    pub secondary: Color32,
    /// Days outside the current month.
    pub tertiary: Color32,
    pub selection: Color32,
    /// Round toolbar buttons (+, ‹, Today, ›).
    pub button: Color32,
    /// Segmented Day/Week/Month/Year control.
    pub seg_track: Color32,
    pub seg_knob: Color32,
    /// Event popover / inspector background, its grouped cards, and the
    /// placeholder text in empty fields.
    pub popover: Color32,
    pub card: Color32,
    pub placeholder: Color32,
}

impl Palette {
    pub fn get(ctx: &egui::Context) -> Self {
        if ctx.style().visuals.dark_mode {
            Palette {
                dark: true,
                bg: Color32::from_rgb(0x24, 0x28, 0x2B),
                sidebar: Color32::from_rgb(0x1D, 0x20, 0x22),
                toolbar: Color32::from_rgb(0x24, 0x28, 0x2B),
                grid: Color32::from_rgb(0x3A, 0x3D, 0x40),
                grid_strong: Color32::from_rgb(0x4A, 0x4D, 0x50),
                weekend: Color32::from_rgb(0x2B, 0x2E, 0x30),
                off_hours: Color32::from_rgb(0x1C, 0x1F, 0x21),
                text: Color32::from_rgb(0xDE, 0xDE, 0xDF),
                secondary: Color32::from_rgb(0x9D, 0x9F, 0x9F),
                tertiary: Color32::from_rgb(0x5A, 0x5D, 0x60),
                selection: Color32::from_rgb(0x32, 0x35, 0x37),
                button: Color32::from_rgb(0x35, 0x39, 0x3B),
                seg_track: Color32::from_rgb(0x1C, 0x1F, 0x21),
                seg_knob: Color32::from_rgb(0x3A, 0x3D, 0x40),
                popover: Color32::from_rgb(0x1F, 0x22, 0x24),
                card: Color32::from_rgb(0x29, 0x2B, 0x2E),
                placeholder: Color32::from_rgb(0x6A, 0x6C, 0x6F),
            }
        } else {
            Palette {
                dark: false,
                bg: Color32::WHITE,
                sidebar: Color32::from_rgb(0xF2, 0xF1, 0xF6),
                toolbar: Color32::WHITE,
                grid: Color32::from_rgb(0xE6, 0xE6, 0xE6),
                grid_strong: Color32::from_rgb(0xD1, 0xD1, 0xD6),
                weekend: Color32::from_rgb(0xF7, 0xF7, 0xF7),
                off_hours: Color32::from_rgb(0xF2, 0xF2, 0xF4),
                text: Color32::from_rgb(0x1D, 0x1D, 0x1F),
                secondary: Color32::from_rgb(0x8E, 0x8E, 0x93),
                tertiary: Color32::from_rgb(0xC7, 0xC7, 0xCC),
                selection: Color32::from_rgb(0xE8, 0xE8, 0xED),
                button: Color32::from_rgb(0xE9, 0xE9, 0xEB),
                seg_track: Color32::from_rgb(0xE3, 0xE3, 0xE8),
                seg_knob: Color32::WHITE,
                popover: Color32::from_rgb(0xF6, 0xF6, 0xF6),
                card: Color32::WHITE,
                placeholder: Color32::from_rgb(0xA8, 0xA8, 0xAD),
            }
        }
    }

    /// Fill behind events. In dark mode Apple uses the calendar color scaled
    /// down to about 30% brightness (e.g. #DC38FF → #421251).
    pub fn event_fill(&self, c: Color32, selected: bool) -> Color32 {
        if selected {
            return c;
        }
        if self.dark {
            scale(c, 0.30)
        } else {
            blend(self.bg, c, 0.18)
        }
    }

    /// Event text: the calendar color itself on the dark fill (a darker shade
    /// in light mode), white on a selected event.
    pub fn event_text(&self, c: Color32, selected: bool) -> Color32 {
        if selected {
            return Color32::WHITE;
        }
        if self.dark {
            // Very dark calendar colors would vanish on the fill; lift them.
            if luminance(c) < 0.35 {
                blend(c, Color32::WHITE, 0.35)
            } else {
                c
            }
        } else {
            blend(c, Color32::BLACK, 0.45)
        }
    }
}

fn scale(c: Color32, f: f32) -> Color32 {
    let s = |x: u8| (x as f32 * f).round() as u8;
    Color32::from_rgb(s(c.r()), s(c.g()), s(c.b()))
}

/// Perceived brightness, 0–1.
fn luminance(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
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
