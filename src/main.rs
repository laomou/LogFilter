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

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LogFilter")
            .with_inner_size([1400.0, 900.0])
            .with_maximized(true),
        ..Default::default()
    };

    eframe::run_native(
        "LogFilter",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, initial_file.clone())))),
    )
}
