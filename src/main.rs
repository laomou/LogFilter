#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod adb;
mod app;
mod config;
mod filter;
mod model;
mod parser;

use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let initial_file: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LogFilter")
        .with_inner_size([1350.0, 720.0])
        // Start maximized so the table fills the screen by default; pairs with
        // ui_table's fill-all-space sizing (max_scroll_height = INFINITY) so the
        // log view uses the whole window instead of a centered 1350×720 box.
        .with_maximized(true)
        .with_maximized(true);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "LogFilter",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, initial_file.clone())))),
    )
}

/// Decode the bundled .ico into egui's RGBA `IconData` for the window/taskbar
/// icon. Returns None if decoding fails so startup never aborts over an icon.
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.ico");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData { rgba: image.into_raw(), width, height })
}
