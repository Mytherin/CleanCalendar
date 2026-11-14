#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod app_icon;
mod date_icon;
mod editor;
mod dates;
mod db;
mod google;
mod interact;
mod layout;
mod model;
mod sync;
mod theme;
mod views;

fn main() -> eframe::Result {
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
