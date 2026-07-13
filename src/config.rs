use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub window: WindowConfig,
    pub view: ViewConfig,
    pub filters: FiltersConfig,
    pub colors: ColorsConfig,
    pub adb: AdbConfig,
    pub recent: RecentConfig,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: f32,
    pub height: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { width: 1100.0, height: 732.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewConfig {
    pub font_size: f32,
    pub columns: [f32; 9],
    pub encoding: String,
    /// File stem of the user font to use as the *primary* face for both the
    /// Proportional and Monospace families (e.g. "SarasaMonoSC-Regular"). Empty
    /// = no primary; all loaded fonts are appended as fallbacks in filename
    /// order and egui's built-in fonts stay primary.
    pub font: String,
    /// UI language: "auto" (detect from system locale), "en", or "zh".
    pub lang: String,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            columns: [50.0, 50.0, 100.0, 20.0, 50.0, 50.0, 100.0, 0.0, 600.0],
            encoding: "utf-8".into(),
            font: String::new(),
            lang: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FiltersConfig {
    pub find: String,
    pub remove: String,
    pub highlight: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub level_v: String,
    pub level_d: String,
    pub level_i: String,
    pub level_w: String,
    pub level_e: String,
    pub level_f: String,
    pub highlights: Vec<String>,
}

impl Default for ColorsConfig {
    fn default() -> Self {
        Self {
            level_v: "0x000000".into(),
            level_d: "0x0000AA".into(),
            level_i: "0x009A00".into(),
            level_w: "0xFF9A00".into(),
            level_e: "0xFF0000".into(),
            level_f: "0xFF0000".into(),
            highlights: vec!["0xFFFF00".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdbConfig {
    pub commands: Vec<String>,
    pub adb_path: Option<String>,
}

impl Default for AdbConfig {
    fn default() -> Self {
        Self {
            commands: vec![
                "logcat -v threadtime".into(),
                "logcat -v time".into(),
                "logcat -b radio -v time".into(),
                "logcat -b events -v time".into(),
                "shell cat /proc/kmsg".into(),
            ],
            adb_path: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentConfig {
    pub files: Vec<PathBuf>,
}

/// Root config directory. Linux: `~/.config/logfilter/`, Windows:
/// `%APPDATA%/logfilter/config/`, macOS: `~/Library/Application Support/logfilter/`.
pub fn config_dir() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "logfilter")?;
    Some(dirs.config_dir().to_path_buf())
}

pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

/// User-supplied font drop-in directory. Any `.ttf` / `.otf` / `.ttc` file here
/// is loaded at startup and registered as a selectable face.
pub fn fonts_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("fonts"))
}

pub fn load() -> Config {
    if let Some(path) = config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return toml::from_str(&text).unwrap_or_default();
        }
    }
    // Fall back to INI migration if the user is launching from the old repo dir.
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(cfg) = import_from_ini(&cwd) {
            let _ = save(&cfg);
            return cfg;
        }
    }
    Config::default()
}

pub fn save(cfg: &Config) -> Result<()> {
    let Some(path) = config_path() else { return Ok(()); };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    std::fs::write(&path, text)?;
    Ok(())
}

pub fn parse_color(s: &str) -> egui::Color32 {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X").trim_start_matches('#');
    let n = u32::from_str_radix(s, 16).unwrap_or(0);
    let r = ((n >> 16) & 0xFF) as u8;
    let g = ((n >> 8) & 0xFF) as u8;
    let b = (n & 0xFF) as u8;
    egui::Color32::from_rgb(r, g, b)
}

/// Very small `java.util.Properties` reader: `key=value` lines, `\` escapes,
/// `#` and `!` comments. Ignores line continuations (Java allows trailing `\`).
fn parse_properties(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') { continue; }
        // Find first unescaped '=' or ':'
        let mut split_at = None;
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' { i += 2; continue; }
            if c == b'=' || c == b':' { split_at = Some(i); break; }
            i += 1;
        }
        let Some(idx) = split_at else { continue; };
        let key = line[..idx].trim().to_string();
        let value_raw = line[idx + 1..].trim_start();
        let mut value = String::with_capacity(value_raw.len());
        let mut chars = value_raw.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some('r') => value.push('\r'),
                    Some(other) => value.push(other),
                    None => {}
                }
            } else {
                value.push(c);
            }
        }
        out.insert(key, value);
    }
    out
}

/// Look for LogFilter.ini + siblings in `dir`; if present, produce a new Config
/// mirroring the Java Properties values. Missing keys keep Config defaults.
pub fn import_from_ini(dir: &std::path::Path) -> Option<Config> {
    let main = dir.join("LogFilter.ini");
    if !main.exists() { return None; }
    let mut cfg = Config::default();

    if let Ok(text) = std::fs::read_to_string(&main) {
        let p = parse_properties(&text);
        if let Some(v) = p.get("INI_WIDTH").and_then(|s| s.parse().ok()) { cfg.window.width = v; }
        if let Some(v) = p.get("INI_HEIGHT").and_then(|s| s.parse().ok()) { cfg.window.height = v; }
        for i in 0..9 {
            if let Some(v) = p.get(&format!("INI_COMUMN_{i}")).and_then(|s| s.parse().ok()) {
                cfg.view.columns[i] = v;
            }
        }
        for (java_key, dst) in [
            ("WORD_FIND", &mut cfg.filters.find),
            ("WORD_REMOVE", &mut cfg.filters.remove),
            ("HIGHLIGHT", &mut cfg.filters.highlight),
        ] {
            if let Some(v) = p.get(java_key) { *dst = v.clone(); }
        }
    }

    if let Ok(text) = std::fs::read_to_string(dir.join("LogFilterColor.ini")) {
        let p = parse_properties(&text);
        for (java_key, dst) in [
            ("INI_COLOR_0", &mut cfg.colors.level_v),
            ("INI_COLOR_7(D)", &mut cfg.colors.level_d),
            ("INI_COLOR_6(I)", &mut cfg.colors.level_i),
            ("INI_COLOR_4(W)", &mut cfg.colors.level_w),
            ("INI_COLOR_3(E)", &mut cfg.colors.level_e),
            ("INI_COLOR_8(F)", &mut cfg.colors.level_f),
        ] {
            if let Some(v) = p.get(java_key) { *dst = v.clone(); }
        }
        let count: usize = p.get("INI_HIGILIGHT_COUNT").and_then(|s| s.parse().ok()).unwrap_or(0);
        if count > 0 {
            let mut hls = Vec::with_capacity(count);
            for i in 0..count {
                if let Some(v) = p.get(&format!("INI_HIGILIGHT_{i}")) {
                    hls.push(v.clone());
                }
            }
            if !hls.is_empty() { cfg.colors.highlights = hls; }
        }
    }

    if let Ok(text) = std::fs::read_to_string(dir.join("LogFilterCmd.ini")) {
        let p = parse_properties(&text);
        let count: usize = p.get("CMD_COUNT").and_then(|s| s.parse().ok()).unwrap_or(0);
        if count > 0 {
            let mut cmds = Vec::with_capacity(count);
            for i in 0..count {
                if let Some(v) = p.get(&format!("CMD_{i}")) {
                    cmds.push(v.clone());
                }
            }
            if !cmds.is_empty() { cfg.adb.commands = cmds; }
        }
    }

    if let Ok(text) = std::fs::read_to_string(dir.join("RecentFile.ini")) {
        cfg.recent.files = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
    }

    Some(cfg)
}

pub fn add_recent(cfg: &mut Config, path: &std::path::Path) {
    cfg.recent.files.retain(|p| p != path);
    cfg.recent.files.insert(0, path.to_path_buf());
    cfg.recent.files.truncate(10);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_basic_kv() {
        let p = parse_properties("A=1\nB : two\n#comment\n!bang\nC=x\\:y");
        assert_eq!(p.get("A").unwrap(), "1");
        assert_eq!(p.get("B").unwrap(), "two");
        assert!(!p.contains_key("#comment"));
        assert_eq!(p.get("C").unwrap(), "x:y");
    }

    #[test]
    fn ini_migration_reads_main_ini() {
        let dir = tempdir_new();
        std::fs::write(
            dir.join("LogFilter.ini"),
            "INI_WIDTH=1200\nINI_HEIGHT=800\nWORD_FIND=hello\nINI_COMUMN_0=42\n",
        ).unwrap();
        let cfg = import_from_ini(&dir).expect("main ini present");
        assert_eq!(cfg.window.width, 1200.0);
        assert_eq!(cfg.window.height, 800.0);
        assert_eq!(cfg.filters.find, "hello");
        assert_eq!(cfg.view.columns[0], 42.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir_new() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lf_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }
}
