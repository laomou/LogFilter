use crate::config;

pub fn tune_table_visuals(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.visuals.widgets.hovered.expansion = 0.0;
        style.interaction.selectable_labels = false;
    });
}

pub fn init_i18n() {
    let en = include_str!("../assets/i18n/en-US.egl");
    let zh = include_str!("../assets/i18n/zh-CN.egl");
    egui_i18n::set_fallback("en-US");
    egui_i18n::load_translations_from_text("en-US", en).unwrap();
    egui_i18n::load_translations_from_text("zh-CN", zh).unwrap();
}

pub fn resolve_startup_lang(stored: &str) {
    let code = match stored {
        "zh" => "zh-CN",
        "en" => "en-US",
        _ => {
            let loc = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
            if loc.starts_with("zh") { "zh-CN" } else { "en-US" }
        }
    };
    egui_i18n::set_language(code);
}

/// Load user-supplied fonts from `~/.config/logfilter/fonts/`. No fonts are
/// embedded and no system paths are probed.
///
/// `primary` is the file stem of the font to make the active face for the
/// Monospace family — i.e. the log table (which renders via FontId::monospace).
/// The menu, panels, and status bar keep egui's default proportional face, so
/// changing the table font does not restyle the rest of the UI. Empty, or a
/// stem that isn't among the loaded fonts, means no selection: egui's built-in
/// monospace default stays active.
///
/// Each loaded font is also exposed under its own `FontFamily::Name(stem)` so
/// the Font menu can render each entry as a preview in that font.
///
/// Returns the list of registered font stems, so callers can track which
/// `FontFamily::Name(..)` values are safe to use for previews (using an
/// unregistered named family panics inside egui).
/// Open `path` in the platform file manager. Best-effort: a missing opener or
/// a spawn failure is silently ignored (there is no sensible UI recovery).
pub fn open_dir(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener).arg(path).spawn();
}

/// Middle-ellipsize `s` so its rendered Body-text width fits within `max_width`
/// points, keeping the head and tail (extension stays visible) with `…` in the
/// middle. Width is measured against the actual font, so it adapts to font size
/// and DPI/resolution automatically. Splits only on `char` boundaries.
pub fn fit_middle(ui: &egui::Ui, s: &str, max_width: f32) -> String {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let width = |t: &str| -> f32 {
        ui.painter().layout_no_wrap(t.to_string(), font.clone(), egui::Color32::WHITE).size().x
    };
    if width(s) <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let join = |keep: usize| -> String {
        let head_len = (keep + 1) / 2;
        let tail_len = keep - head_len;
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        format!("{head}…{tail}")
    };
    // Binary-search the largest `keep` (head+tail char count) that still fits.
    let (mut lo, mut hi, mut best) = (0usize, chars.len().saturating_sub(1), String::from("…"));
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let cand = join(mid);
        if width(&cand) <= max_width {
            best = cand;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

/// Scan config/fonts/ and return (file_stem, display_name) for every supported
/// font file found there. No bytes are loaded — just metadata for the menu.
pub fn list_user_font_stems() -> Vec<(String, String)> {
    let Some(dir) = config::fonts_dir() else { return vec![] };
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut entries: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
                Some("ttf") | Some("otf") | Some("ttc") | Some("otc"),
            )
        })
        .collect();
    entries.sort();
    entries
        .into_iter()
        .map(|p| {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("font")
                .to_string();
            // Nicer display name: strip the common CJK size suffix.
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&stem)
                .to_string();
            (stem, name)
        })
        .collect()
}

/// Load built-in fonts + the *single* selected user font (if any) into egui.
/// Other fonts in config/fonts/ stay on disk until selected — this keeps
/// memory proportional to what is actually used, not all installed fonts.
///
/// Loads only the selected font (if any) plus built-in fonts. Other fonts in
/// config/fonts/ stay on disk.
pub fn install_ui_font(ctx: &egui::Context, primary: &str, stems: &[(String, String)]) {
    let mut fonts = egui::FontDefinitions::default();
    // Drop Hack — we use Proportional for the table, Monospace is a mirror.
    fonts.font_data.remove("Hack");
    for fonts in fonts.families.values_mut() {
        fonts.retain(|name| name != "Hack");
    }
    let mut loaded = false;

    // Load ONLY the selected font (primary), not all fonts in the directory.
    if !primary.is_empty() {
        if let Some((_, path)) = find_font_file(stems, primary) {
            if let Ok(bytes) = std::fs::read(&path) {
                let name = primary.to_string();
                fonts.font_data.insert(
                    name.clone(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                fonts.families.insert(
                    egui::FontFamily::Name(name.clone().into()),
                    vec![name],
                );
                loaded = true;
            }
        }
    }

    // Mirror Proportional → Monospace so the table matches the menu chrome.
    if let Some(prop) = fonts.families.get(&egui::FontFamily::Proportional).cloned() {
        fonts.families.insert(egui::FontFamily::Monospace, prop);
    }
    // If a primary font was loaded, prepend it to the Monospace stack.
    if loaded {
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, primary.to_string());
    }
    ctx.set_fonts(fonts);
}

/// Find a font's path on disk given its file stem and the stems list.
pub fn find_font_file(stems: &[(String, String)], stem: &str) -> Option<(usize, std::path::PathBuf)> {
    let dir = config::fonts_dir()?;
    let pos = stems.iter().position(|(s, _)| s == stem)?;
    // Reconstruct the path from the stored display name.
    let file_name = &stems[pos].1;
    Some((pos, dir.join(file_name)))
}

/// egui's stock text sizes (Body 14, Button 14, Small 10) render a bit small on
/// modern high-DPI displays; bump them up ~1pt so menus/toolbars/status match the
/// table density chosen via View → Font size (default 14).
pub fn bump_global_text_sizes(ctx: &egui::Context) {
    use egui::TextStyle;
    ctx.all_styles_mut(|style| {
        for (style_key, size) in [
            (TextStyle::Body, 13.0),
            (TextStyle::Button, 13.0),
            (TextStyle::Monospace, 14.0),
            (TextStyle::Small, 12.0),
            (TextStyle::Heading, 20.0),
        ] {
            if let Some(id) = style.text_styles.get_mut(&style_key) {
                id.size = size;
            }
        }
    });
}

