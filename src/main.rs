#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai;
mod app;
mod config;
mod game;
mod hub;
mod invite;
mod net;
mod ops;
mod upnp;
mod remote;
mod selfupdate;
mod shortcuts;
mod steam;
mod util;
mod vdf;

use eframe::egui;

fn main() -> eframe::Result {
    // "Start the server when this computer starts": run the hub, no window.
    if std::env::args().any(|a| a == "--start-hub") {
        if let Err(e) = hub::start(&hub::computer_name()) {
            let _ = std::fs::write(config::sub("logs").join("hub-autostart.log"), format!("{e:#}"));
        }
        return Ok(());
    }
    selfupdate::cleanup();
    // Leftovers of installs that were interrupted (power off, crash).
    let _ = std::fs::remove_dir_all(config::data_dir().join("staging"));
    let deck = steam::is_steam_deck();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("NightCoop Helper")
        .with_app_id("nightcoop-helper")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([760.0, 480.0]);
    if deck {
        // The Deck's 1280×800 screen: use all of it.
        viewport = viewport.with_maximized(true);
    }
    let options = eframe::NativeOptions {
        viewport,
        // Settings live in our portable data folder, not eframe's storage.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native("NightCoop Helper", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
