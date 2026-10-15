use crate::config;
pub fn list_user_font_stems() -> Vec<(String, String)> {
    let Some(dir) = config::fonts_dir() else {
        return vec![];
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut entries: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .as_deref(),
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
                fonts
                    .families
                    .insert(egui::FontFamily::Name(name.clone().into()), vec![name]);
                loaded = true;
            }
        }
    }

    // egui's built-in fonts carry no CJK glyphs, so without this every
    // Chinese/Japanese/Korean character renders as a tofu box unless the user
    // manually picks a CJK font. Append a system CJK font as a fallback in the
    // Proportional family; the mirror below copies it into Monospace too, so both
    // the menu chrome and the table show CJK out of the box.
    push_cjk_fallback_from(&mut fonts, system_cjk_candidates());

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

/// System fonts that carry CJK (and broad Unicode) glyphs, tried in order. Used
/// as a fallback so CJK text renders without the user importing a font.
fn system_cjk_candidates() -> &'static [(&'static str, u32)] {
    #[cfg(target_os = "windows")]
    {
        &[
            (r"C:\Windows\Fonts\msyh.ttc", 0), // Microsoft YaHei (SC)
            (r"C:\Windows\Fonts\msyh.ttf", 0),
            (r"C:\Windows\Fonts\simhei.ttf", 0), // SimHei
            (r"C:\Windows\Fonts\simsun.ttc", 0), // SimSun
            (r"C:\Windows\Fonts\msjh.ttc", 0),   // MS JhengHei (TC)
            (r"C:\Windows\Fonts\meiryo.ttc", 0), // Meiryo (JP)
            (r"C:\Windows\Fonts\malgun.ttf", 0), // Malgun Gothic (KR)
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            ("/System/Library/Fonts/PingFang.ttc", 0),
            ("/System/Library/Fonts/STHeiti Light.ttc", 0),
            ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
            ("/Library/Fonts/Arial Unicode.ttf", 0),
        ]
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        &[
            ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
            ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0),
            (
                "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
                0,
            ),
            ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
            (
                "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
                0,
            ),
        ]
    }
}

/// Load the first available font in `candidates` and append it as a fallback in
/// the Proportional family (so it fills in glyphs the earlier fonts lack, e.g.
/// CJK). Returns true if one was loaded. `.1` is the face index for `.ttc`
/// collections. Kept separate + parameterized so it can be unit-tested.
fn push_cjk_fallback_from(fonts: &mut egui::FontDefinitions, candidates: &[(&str, u32)]) -> bool {
    for (path, index) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let name = "system-cjk".to_string();
            let mut data = egui::FontData::from_owned(bytes);
            data.index = *index;
            fonts
                .font_data
                .insert(name.clone(), std::sync::Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(name);
            return true;
        }
    }
    false
}

/// Find a font's path on disk given its file stem and the stems list.
pub fn find_font_file(
    stems: &[(String, String)],
    stem: &str,
) -> Option<(usize, std::path::PathBuf)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_candidates_are_listed_for_this_platform() {
        // Every supported platform should offer at least one candidate path.
        assert!(!system_cjk_candidates().is_empty());
    }

    #[test]
    fn cjk_fallback_no_op_when_nothing_found() {
        let mut fonts = egui::FontDefinitions::default();
        let before = fonts.font_data.len();
        assert!(!push_cjk_fallback_from(
            &mut fonts,
            &[("/no/such/font-xyz.ttf", 0)]
        ));
        assert_eq!(fonts.font_data.len(), before, "nothing added on a miss");
    }

    #[test]
    fn cjk_fallback_appends_to_proportional_when_font_exists() {
        // Only asserts the positive path if a real CJK font is present (it is on
        // most Linux dev/CI images via Noto); otherwise the miss path above covers it.
        let real = system_cjk_candidates()
            .iter()
            .find(|(p, _)| std::path::Path::new(p).exists());
        let Some(&(path, index)) = real else {
            return;
        };
        let mut fonts = egui::FontDefinitions::default();
        assert!(push_cjk_fallback_from(&mut fonts, &[(path, index)]));
        assert!(fonts.font_data.contains_key("system-cjk"));
        assert!(
            fonts.families[&egui::FontFamily::Proportional]
                .iter()
                .any(|n| n == "system-cjk"),
            "CJK font must be a Proportional fallback"
        );
    }
}
