#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod adb;
mod app;
mod config;
mod filter;
mod fonts;
mod io;
mod lock;
mod model;
mod parser;

use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let initial_file: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);
    // Load once so the saved dimensions used by the native viewport and the
    // configuration owned by App can never diverge.
    let cfg = config::load();

    let title = format!("LogFilter v{}", env!("CARGO_PKG_VERSION"));
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size(restored_window_size(&cfg));
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
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cfg.clone(), initial_file.clone())))),
    )
}

/// Return a safe viewport size from persisted configuration. Invalid or
/// implausible values can otherwise create an invisible or unusable window
/// after a manually edited/corrupted config file.
fn restored_window_size(cfg: &config::Config) -> [f32; 2] {
    const DEFAULT: [f32; 2] = [1350.0, 720.0];
    const MIN: f32 = 320.0;
    const MAX: f32 = 10_000.0;

    let valid = |value: f32| value.is_finite() && (MIN..=MAX).contains(&value);
    if valid(cfg.window.width) && valid(cfg.window.height) {
        [cfg.window.width, cfg.window.height]
    } else {
        DEFAULT
    }
}

/// Decode the bundled .ico into egui's RGBA `IconData` for the window/taskbar
/// icon. Returns None if decoding fails so startup never aborts over an icon.
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.ico");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData { rgba: image.into_raw(), width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_window_size_uses_saved_valid_dimensions() {
        let mut cfg = config::Config::default();
        cfg.window.width = 1600.0;
        cfg.window.height = 900.0;
        assert_eq!(restored_window_size(&cfg), [1600.0, 900.0]);
    }

    #[test]
    fn restored_window_size_rejects_invalid_dimensions() {
        let mut cfg = config::Config::default();
        cfg.window.width = f32::NAN;
        cfg.window.height = 900.0;
        assert_eq!(restored_window_size(&cfg), [1350.0, 720.0]);

        cfg.window.width = 1600.0;
        cfg.window.height = 20_000.0;
        assert_eq!(restored_window_size(&cfg), [1350.0, 720.0]);
    }
}
