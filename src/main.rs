#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod config;
mod devices;
mod hotkey;
mod icon_art;
mod model;
mod record;
mod theme;
mod tray;
mod waveform;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 620.0])
            .with_min_inner_size([620.0, 460.0])
            .with_title("Open Soundboard"),
        ..Default::default()
    };

    eframe::run_native(
        "Open Soundboard",
        options,
        Box::new(|cc| Ok(Box::new(app::SoundboardApp::new(cc)))),
    )
}
