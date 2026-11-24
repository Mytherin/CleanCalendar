#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// The label for a shortcut on egui's "command" modifier (`Modifiers::command`):
/// `cmd_key!("F")` is "⌘F" on macOS and "Ctrl+F" elsewhere.
macro_rules! cmd_key {
    ($key:literal) => {
        if cfg!(target_os = "macos") {
            concat!("\u{2318}", $key)
        } else {
            concat!("Ctrl+", $key)
        }
    };
}

mod app;
mod app_icon;
mod caldav;
mod date_icon;
mod editor;
mod dates;
mod db;
mod fonts;
mod google;
mod interact;
mod layout;
mod menu;
mod model;
mod places;
mod recurrence;
mod remote;
mod search;
mod settings;
mod sync;
mod text_select;
mod theme;
mod views;

fn main() -> eframe::Result {
    #[cfg(feature = "build-places")]
    if let Some(code) = places::build::main_if_asked() {
        std::process::exit(code);
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Calendar")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(date_icon::icon_data(chrono::Local::now().date_naive(), 256)),
        ..Default::default()
    };
    eframe::run_native("CleanCalendar", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
