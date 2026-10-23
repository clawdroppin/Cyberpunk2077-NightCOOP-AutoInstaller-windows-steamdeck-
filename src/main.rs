#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod config;
mod game;
mod ops;
mod remote;
mod selfupdate;
mod shortcuts;
mod steam;
mod util;
mod vdf;

use eframe::egui;

fn main() -> eframe::Result {
    selfupdate::cleanup();
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
