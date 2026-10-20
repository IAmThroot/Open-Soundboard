#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod config;
mod devices;
mod hotkey;
mod icon_art;
mod instance;
mod model;
mod paths;
mod record;
mod theme;
mod tray;
mod waveform;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Held for the whole run; the lock is released by the OS on exit.
    let _instance = match instance::acquire() {
        Ok(lock) => lock,
        Err(message) => {
            log::error!("{message}");
            let _ = rfd::MessageDialog::new()
                .set_title("Open Soundboard")
                .set_description(message)
                .set_level(rfd::MessageLevel::Warning)
                .show();
            return Ok(());
        }
    };

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
