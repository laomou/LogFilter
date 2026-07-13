use crate::config;
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
