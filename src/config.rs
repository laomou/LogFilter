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
        Self {
            width: 1100.0,
            height: 732.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewConfig {
    pub font_size: f32,
    pub columns: [f32; 10],
    /// Per-column visibility, same order as `columns`:
    /// line, date, time, level, pid, thread, uid, tag, bookmark, message.
    pub columns_visible: [bool; 10],
    pub encoding: String,
    /// File stem of the user font to use as the *primary* face for both the
    /// Proportional and Monospace families (e.g. "SarasaMonoSC-Regular"). Empty
    /// = no primary; all loaded fonts are appended as fallbacks in filename
    /// order and egui's built-in fonts stay primary.
    pub font: String,
    /// UI language: "auto" (detect from system locale), "en", or "zh".
    pub lang: String,
    /// Color theme: "light" or "dark".
    pub theme: String,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            columns: [50.0, 50.0, 100.0, 20.0, 50.0, 50.0, 50.0, 100.0, 0.0, 600.0],
            // line, date, time, level, pid, thread, uid, tag, bookmark, message —
            // UID and the bookmark column are hidden by default.
            columns_visible: [true, true, true, true, true, true, false, true, false, true],
            encoding: "utf-8".into(),
            font: String::new(),
            lang: "auto".into(),
            theme: "light".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FiltersConfig {
    pub find: String,
    pub remove: String,
    pub highlight: String,
    /// Enabled state of each filter. `None` = field absent in an older config →
    /// fall back to "on when the text is non-empty" (the historical behavior).
    pub find_on: Option<bool>,
    pub remove_on: Option<bool>,
    pub highlight_on: Option<bool>,
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
        Self::light_defaults()
    }
}

impl ColorsConfig {
    pub fn light_defaults() -> Self {
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

    pub fn migrate(&mut self, old: &Self, new: &Self) {
        for (cur, o, n) in [
            (&mut self.level_v, &old.level_v, &new.level_v),
            (&mut self.level_d, &old.level_d, &new.level_d),
            (&mut self.level_i, &old.level_i, &new.level_i),
            (&mut self.level_w, &old.level_w, &new.level_w),
            (&mut self.level_e, &old.level_e, &new.level_e),
            (&mut self.level_f, &old.level_f, &new.level_f),
        ] {
            if *cur == *o {
                *cur = n.clone();
            }
        }
        if self.highlights == old.highlights {
            self.highlights = new.highlights.clone();
        }
    }

    pub fn dark_defaults() -> Self {
        Self {
            level_v: "0x888888".into(),
            level_d: "0x5599FF".into(),
            level_i: "0x48B048".into(),
            level_w: "0xFFBB33".into(),
            level_e: "0xFF5555".into(),
            level_f: "0xFF55FF".into(),
            highlights: vec!["0x665500".into()],
        }
    }
}

use crate::transport::Transport;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdbConfig {
    pub commands: Vec<String>,
    pub adb_path: Option<String>,
    /// Path to the hdc binary (HarmonyOS). Empty/None = resolve "hdc" on PATH.
    pub hdc_path: Option<String>,
    /// Selected device-connector backend (adb / hdc).
    pub transport: Transport,
    /// Last-used adb command and device, restored on the next launch. Empty =
    /// fall back to the first command / "(any)" device.
    pub selected_cmd: String,
    pub selected_device: String,
}

impl Default for AdbConfig {
    fn default() -> Self {
        Self {
            commands: vec![
                "logcat -v threadtime".into(),
                "logcat -v time".into(),
                "logcat -b radio -v time".into(),
                "logcat -b events -v time".into(),
                // HarmonyOS hilog (select the HarmonyOS transport to run via hdc).
                "hilog -v threadtime -r".into(),
                "hilog -v time -r".into(),
                "hilog -D 0x2F00 -v time".into(),
                "hilog -D 0x2D00 -v time".into(),
                // Kernel log — works via adb shell or hdc shell.
                "shell cat /proc/kmsg".into(),
            ],
            adb_path: None,
            hdc_path: None,
            transport: Transport::Adb,
            selected_cmd: String::new(),
            selected_device: String::new(),
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
    let mut cfg = load_raw();
    ensure_builtin_commands(&mut cfg);
    cfg
}

fn load_raw() -> Config {
    if let Some(path) = config_path() {
        if let Some(cfg) = read_config(&path) {
            return cfg;
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

/// Ensure built-in commands introduced in newer versions are present even in an
/// older saved config — the command combo isn't editable, so a user couldn't add
/// them otherwise. Idempotent (won't duplicate).
fn ensure_builtin_commands(cfg: &mut Config) {
    for cmd in [
        "hilog -v threadtime -r",
        "hilog -v time -r",
        "hilog -D 0x2F00 -v time",
        "hilog -D 0x2D00 -v time",
    ] {
        if !cfg.adb.commands.iter().any(|c| c == cmd) {
            cfg.adb.commands.push(cmd.to_string());
        }
    }
}

/// Read and parse a config file. Returns `None` if it's missing or unparseable.
/// On a parse error the bad file is preserved as `.bak` instead of being left to
/// be silently overwritten with defaults — otherwise a single bad value (or a
/// truncated write) would lose every setting for good.
fn read_config(path: &std::path::Path) -> Option<Config> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&text) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            let bak = path.with_extension("toml.bak");
            let _ = std::fs::rename(path, &bak);
            eprintln!(
                "logfilter: failed to parse config at {} ({e}); backed up to {} and reset to defaults",
                path.display(),
                bak.display()
            );
            None
        }
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    write_config(&path, cfg)
}

/// Write the config atomically: a crash/power-loss mid-write would otherwise
/// leave a truncated (unparseable) file that wipes every setting on next launch.
/// Write to a sibling temp file, then rename over the target.
fn write_config(path: &std::path::Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn parse_color(s: &str) -> egui::Color32 {
    let s = s
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .trim_start_matches('#');
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
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        // Find first unescaped '=' or ':'
        let mut split_at = None;
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'=' || c == b':' {
                split_at = Some(i);
                break;
            }
            i += 1;
        }
        let Some(idx) = split_at else {
            continue;
        };
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
    if !main.exists() {
        return None;
    }
    let mut cfg = Config::default();

    if let Ok(text) = std::fs::read_to_string(&main) {
        let p = parse_properties(&text);
        if let Some(v) = p.get("INI_WIDTH").and_then(|s| s.parse().ok()) {
            cfg.window.width = v;
        }
        if let Some(v) = p.get("INI_HEIGHT").and_then(|s| s.parse().ok()) {
            cfg.window.height = v;
        }
        for i in 0..10 {
            if let Some(v) = p
                .get(&format!("INI_COMUMN_{i}"))
                .and_then(|s| s.parse().ok())
            {
                cfg.view.columns[i] = v;
            }
        }
        for (java_key, dst) in [
            ("WORD_FIND", &mut cfg.filters.find),
            ("WORD_REMOVE", &mut cfg.filters.remove),
            ("HIGHLIGHT", &mut cfg.filters.highlight),
        ] {
            if let Some(v) = p.get(java_key) {
                *dst = v.clone();
            }
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
            if let Some(v) = p.get(java_key) {
                *dst = v.clone();
            }
        }
        let count: usize = p
            .get("INI_HIGILIGHT_COUNT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if count > 0 {
            let mut hls = Vec::with_capacity(count);
            for i in 0..count {
                if let Some(v) = p.get(&format!("INI_HIGILIGHT_{i}")) {
                    hls.push(v.clone());
                }
            }
            if !hls.is_empty() {
                cfg.colors.highlights = hls;
            }
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
            if !cmds.is_empty() {
                cfg.adb.commands = cmds;
            }
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

/// Drop recent-files entries whose file no longer exists, so the list doesn't
/// accumulate dead links that only error when clicked. Returns true if anything
/// was removed.
pub fn prune_missing_recent(cfg: &mut Config) -> bool {
    let before = cfg.recent.files.len();
    cfg.recent.files.retain(|p| p.exists());
    cfg.recent.files.len() != before
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
        )
        .unwrap();
        let cfg = import_from_ini(&dir).expect("main ini present");
        assert_eq!(cfg.window.width, 1200.0);
        assert_eq!(cfg.window.height, 800.0);
        assert_eq!(cfg.filters.find, "hello");
        assert_eq!(cfg.view.columns[0], 42.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_builtin_commands_injects_hilog_into_old_config() {
        // An older saved config without the hilog command should get it added.
        let mut cfg = Config::default();
        cfg.adb.commands = vec!["logcat -v threadtime".into()];
        ensure_builtin_commands(&mut cfg);
        assert!(
            cfg.adb.commands.iter().any(|c| c.contains("hilog")),
            "hilog command added"
        );
        // Idempotent — a second call doesn't duplicate.
        let n = cfg.adb.commands.len();
        ensure_builtin_commands(&mut cfg);
        assert_eq!(cfg.adb.commands.len(), n, "no duplicate on re-run");
    }

    #[test]
    fn transport_roundtrips_and_defaults_to_adb() {
        assert_eq!(AdbConfig::default().transport, Transport::Adb);
        // An older config without the transport field loads as adb.
        let old: Config = toml::from_str("[adb]\n").unwrap();
        assert_eq!(old.adb.transport, Transport::Adb);
        // Round-trips hdc + the hdc path.
        let mut cfg = Config::default();
        cfg.adb.transport = Transport::Hdc;
        cfg.adb.hdc_path = Some("/opt/harmony/hdc".into());
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.adb.transport, Transport::Hdc);
        assert_eq!(back.adb.hdc_path.as_deref(), Some("/opt/harmony/hdc"));
    }

    #[test]
    fn config_toml_roundtrips_new_persisted_fields() {
        let mut cfg = Config::default();
        cfg.view.columns_visible = [
            false, true, false, true, false, true, true, false, true, false,
        ];
        cfg.filters.find_on = Some(false);
        cfg.filters.highlight_on = Some(true);
        cfg.adb.selected_cmd = "logcat -b radio -v time".into();
        cfg.adb.selected_device = "emulator-5554".into();

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.view.columns_visible, cfg.view.columns_visible);
        assert_eq!(back.filters.find_on, Some(false));
        assert_eq!(back.filters.highlight_on, Some(true));
        assert_eq!(back.adb.selected_cmd, "logcat -b radio -v time");
        assert_eq!(back.adb.selected_device, "emulator-5554");
    }

    #[test]
    fn old_config_missing_new_fields_uses_defaults() {
        // An older config that predates the new fields must still load, with the
        // new fields defaulting (find_on = None so the app infers from the text).
        let cfg: Config = toml::from_str("[filters]\nfind = \"hello\"\n").unwrap();
        assert_eq!(cfg.filters.find, "hello");
        assert_eq!(cfg.filters.find_on, None);
        assert_eq!(
            cfg.view.columns_visible,
            ViewConfig::default().columns_visible
        );
        assert_eq!(cfg.adb.selected_cmd, "");
    }

    #[test]
    fn write_config_atomic_roundtrips_and_leaves_no_temp() {
        let dir = tempdir_new();
        let path = dir.join("rt_config.toml");
        let mut cfg = Config::default();
        cfg.adb.selected_device = "dev1".into();
        cfg.view.columns_visible[6] = true; // show UID

        write_config(&path, &cfg).unwrap();
        assert!(path.exists(), "config file written");
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "temp file should be renamed away, not left behind"
        );
        let back = read_config(&path).expect("written config parses");
        assert_eq!(back.adb.selected_device, "dev1");
        assert!(back.view.columns_visible[6]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_config_backs_up_corrupt_file() {
        let dir = tempdir_new();
        let path = dir.join("bad_config.toml");
        std::fs::write(&path, "this is = not valid toml ][").unwrap();

        let result = read_config(&path);
        assert!(result.is_none(), "corrupt config should not parse");
        assert!(
            path.with_extension("toml.bak").exists(),
            "corrupt config must be preserved as .bak, not lost"
        );
        assert!(!path.exists(), "the bad file is moved aside");

        let _ = std::fs::remove_file(path.with_extension("toml.bak"));
    }

    #[test]
    fn prune_missing_recent_drops_dead_entries() {
        let dir = tempdir_new();
        let alive = dir.join("alive.log");
        std::fs::write(&alive, "x").unwrap();
        let dead = dir.join("dead.log"); // never created

        let mut cfg = Config::default();
        cfg.recent.files = vec![alive.clone(), dead.clone()];
        let removed = prune_missing_recent(&mut cfg);

        assert!(removed, "a missing entry should be reported as removed");
        assert_eq!(cfg.recent.files, vec![alive.clone()], "dead entry pruned");
        // Nothing to remove the second time.
        assert!(!prune_missing_recent(&mut cfg));

        let _ = std::fs::remove_file(&alive);
    }

    fn tempdir_new() -> std::path::PathBuf {
        // Unique per call so tests don't share a directory — ini_migration_reads_main_ini
        // does remove_dir_all(), which would otherwise race the file-based tests
        // running in parallel and wipe their files mid-test.
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("lf_test_{}_{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }
}
