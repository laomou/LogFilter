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
    pub device: DeviceConfig,
    pub recent: RecentConfig,
    pub recording: RecordingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    /// Zero selects an automatic preview-cache budget. Raw recordings are on disk.
    pub cache_mib: u64,
    pub min_free_mib: u64,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            cache_mib: 0,
            min_free_mib: 1024,
        }
    }
}

/// Where raw captures are written.
///
/// A per-user data directory, not the process working directory: launched from a
/// desktop shortcut or start-menu entry the CWD is the install directory (often
/// not writable), which made recording fail outright, and launching from
/// different shells scattered the recordings across directories.
pub fn recordings_dir() -> Option<PathBuf> {
    Some(
        ProjectDirs::from("", "", "logfilter")?
            .data_local_dir()
            .join("logs"),
    )
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

use crate::transport::{self, Transport};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    /// Command dropdown per backend (adb runs logcat/shell, hdc runs hilog).
    pub adb_commands: Vec<String>,
    pub hdc_commands: Vec<String>,
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

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            adb_commands: transport::default_commands(Transport::Adb),
            hdc_commands: transport::default_commands(Transport::Hdc),
            adb_path: None,
            hdc_path: None,
            transport: Transport::Adb,
            selected_cmd: String::new(),
            selected_device: String::new(),
        }
    }
}

impl DeviceConfig {
    /// The command dropdown list for a backend.
    pub fn commands(&self, transport: Transport) -> &[String] {
        match transport {
            Transport::Adb => &self.adb_commands,
            Transport::Hdc => &self.hdc_commands,
        }
    }

    /// Mutable command list for a backend (to inject missing built-ins).
    fn commands_mut(&mut self, transport: Transport) -> &mut Vec<String> {
        match transport {
            Transport::Adb => &mut self.adb_commands,
            Transport::Hdc => &mut self.hdc_commands,
        }
    }

    /// The user's explicit binary-path override for a backend, if any.
    pub fn binary_override(&self, transport: Transport) -> Option<&str> {
        match transport {
            Transport::Adb => self.adb_path.as_deref(),
            Transport::Hdc => self.hdc_path.as_deref(),
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
    sanitize_view(&mut cfg.view);
    cfg
}

fn load_raw() -> Config {
    if let Some(path) = config_path() {
        match load_config_file(&path) {
            ConfigFile::Loaded(cfg) | ConfigFile::Recovered(cfg) => return cfg,
            // A file that exists but cannot be read (permissions, a sharing
            // violation, a directory in its place) must not be mistaken for a
            // fresh install: falling through to the INI migration would write a
            // config from decade-old values over the real one.
            ConfigFile::Unreadable => return Config::default(),
            ConfigFile::Missing => {}
        }
    }
    // Fall back to INI migration only when there is no config at all.
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(cfg) = import_from_ini(&cwd) {
            let _ = save(&cfg);
            return cfg;
        }
    }
    Config::default()
}

/// What the config file on disk turned out to be.
enum ConfigFile {
    /// Parsed cleanly; nothing to report.
    Loaded(Config),
    /// Some fields were unusable. The readable sections were kept and the bad
    /// file was backed up.
    Recovered(Config),
    /// No config file yet.
    Missing,
    /// The file exists but could not be read.
    Unreadable,
}

fn load_config_file(path: &std::path::Path) -> ConfigFile {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigFile::Missing,
        Err(e) => {
            eprintln!(
                "logfilter: cannot read config at {} ({e}); using defaults for this run",
                path.display()
            );
            return ConfigFile::Unreadable;
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(cfg) => ConfigFile::Loaded(cfg),
        Err(e) => {
            let cfg = recover_config(&text);
            let backup = backup_path(path);
            let kept = move_to_backup(path, &backup);
            // Write the recovered settings back immediately. The rename above
            // moved the only config away, and the caller only saves when the
            // serialized config *differs* from what it loaded — so without this
            // a user who changes nothing is left with no config on disk, and
            // the next launch sees `Missing` and runs the legacy INI import over
            // the recovered settings. That is exactly what the `Unreadable`
            // branch below exists to prevent.
            let restored = write_config(path, &cfg);
            eprintln!(
                "logfilter: failed to parse config at {} ({e}); kept the readable sections{}{}",
                path.display(),
                match &kept {
                    Ok(()) => format!(" and saved the original to {}", backup.display()),
                    Err(rename_error) => format!(" (could not back it up: {rename_error})"),
                },
                match &restored {
                    Ok(()) => String::new(),
                    Err(write_error) => {
                        format!(" (could not write the recovered config: {write_error})")
                    }
                }
            );
            ConfigFile::Recovered(cfg)
        }
    }
}

/// A timestamped backup path, so a second failure cannot destroy the only copy of
/// the user's original settings.
///
/// The name is *reserved*, not merely tested. `exists()` followed by a `rename`
/// by the caller is a race: two instances failing inside the same second both
/// see the same name free, and the second rename then overwrites the first
/// backup — precisely the loss this function exists to prevent. Creating the
/// file with `create_new` makes the winner unambiguous, and the caller's rename
/// replaces the empty placeholder. When that rename fails, the caller removes
/// the placeholder again (see `move_to_backup`), so a failed backup cannot
/// leave behind an empty file that looks like a backup.
///
/// The process id is in the name as well, so concurrent instances do not even
/// contend in the common case.
///
/// Old backups are deliberately never deleted. One accumulates per failed load,
/// which is rare, and each is a copy of settings the user may not have anywhere
/// else; a few kilobytes is the cheaper side of that trade.
fn backup_path(path: &std::path::Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let pid = std::process::id();
    let mut candidate = path.with_file_name(format!("{name}.{stamp}.{pid}.bak"));
    for sequence in 1..1000 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                candidate = path.with_file_name(format!("{name}.{stamp}.{pid}.{sequence}.bak"));
            }
            // The name cannot be reserved for some other reason (read-only
            // directory, no permission). Hand it back and let the caller's
            // rename surface the real problem rather than masking it here.
            Err(_) => return candidate,
        }
    }
    candidate
}

/// Move the corrupt `original` onto its reserved `backup` name.
///
/// When the move fails, the empty placeholder `backup_path` reserved is
/// removed again: a failed rename means the original never left its place,
/// and an empty file named `*.bak` is exactly what a user would mistake for
/// a backup of their settings — the opposite of what this exists to ensure.
fn move_to_backup(original: &std::path::Path, backup: &std::path::Path) -> std::io::Result<()> {
    match std::fs::rename(original, backup) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(backup);
            Err(e)
        }
    }
}

/// Build a config from whatever part of a corrupt file still parses.
///
/// Every section is read independently and falls back to its own default, so one
/// bad field (a mistyped number, an array whose length changed between versions)
/// no longer resets the theme, fonts, filters, colors and recent files with it.
fn recover_config(text: &str) -> Config {
    let mut cfg = Config::default();
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return cfg;
    };
    let Ok(table) = value.try_into::<toml::Table>() else {
        return cfg;
    };
    macro_rules! section {
        ($name:literal, $field:ident) => {
            if let Some(section) = table.get($name) {
                if let Ok(parsed) = section.clone().try_into() {
                    cfg.$field = parsed;
                }
            }
        };
    }
    section!("window", window);
    section!("view", view);
    section!("filters", filters);
    section!("colors", colors);
    section!("device", device);
    section!("recent", recent);
    section!("recording", recording);
    cfg
}

/// Table font-size bounds. These are the *only* definition of a valid font
/// size: the size submenu offers exactly this range, `adjusted_table_font_size`
/// clamps keyboard changes to it, and `sanitize_view` clamps whatever is on
/// disk to it. Keeping one range means a hand-edited value can never land
/// somewhere the UI is unable to represent or change.
pub const MIN_FONT_SIZE: f32 = 13.0;
pub const MAX_FONT_SIZE: f32 = 18.0;

/// Upper bound for a saved column width. Far beyond any usable layout, but
/// finite: an absurd width otherwise propagates into the table layout.
const MAX_COLUMN_WIDTH: f32 = 5000.0;

/// Clamp view values that reach the renderer directly.
///
/// A hand-edited (or truncated, or merged-from-a-backup) config parses as valid
/// TOML while still holding a value no code path can survive: `font_size` is
/// passed straight to `FontId::monospace` and a large enough glyph trips
/// epaint's texture-atlas assertion, i.e. a panic on *every* launch with no way
/// to fix it from inside the app. The recovery path cannot help — the file
/// parses fine, so it is never backed up. Non-finite values are equally fatal
/// (NaN widths collapse the layout) and cannot be clamped, so they fall back to
/// the default. Window size already gets this treatment in `main`.
fn sanitize_view(view: &mut ViewConfig) {
    let defaults = ViewConfig::default();
    view.font_size = if view.font_size.is_finite() {
        view.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    } else {
        defaults.font_size
    };
    for (width, default) in view.columns.iter_mut().zip(defaults.columns) {
        *width = if width.is_finite() {
            width.clamp(0.0, MAX_COLUMN_WIDTH)
        } else {
            default
        };
    }
}

/// Reconcile an older saved config with the built-in command set introduced in
/// newer versions — the command combo isn't user-editable, so this is the only
/// way an existing config picks up new commands. Idempotent.
fn ensure_builtin_commands(cfg: &mut Config) {
    // Add any built-in command an older config is missing, to its backend's list.
    for b in transport::BUILTIN_COMMANDS {
        let list = cfg.device.commands_mut(b.transport);
        if !list.iter().any(|c| c == b.cmd) {
            list.push(b.cmd.to_string());
        }
    }

    // Keep the kernel-log command last in the adb list: injecting the commands
    // above (or an older saved order) can otherwise strand it mid-dropdown.
    const KMSG: &str = "shell cat /proc/kmsg";
    if let Some(pos) = cfg.device.adb_commands.iter().position(|c| c == KMSG) {
        let kmsg = cfg.device.adb_commands.remove(pos);
        cfg.device.adb_commands.push(kmsg);
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    write_config(&path, cfg)
}

/// A per-process sibling path used to replace `path` atomically.
///
/// The process id keeps two instances from fighting over one temp name (a fixed
/// name would let them interleave writes and rename half of each other's data).
pub(crate) fn temp_sibling(path: &std::path::Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("logfilter"));
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Flush the directory entry changed by a create/rename to disk.
///
/// Declared as no-op on non-Unix targets, where a directory cannot be opened as
/// a file; the rename itself is still atomic there.
pub(crate) fn sync_parent_dir(path: &std::path::Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Write the config atomically: a crash/power-loss mid-write would otherwise
/// leave a truncated (unparseable) file that wipes every setting on next launch.
/// Write and `fsync` a sibling temp file, rename it over the target, then flush
/// the directory entry — a rename whose data never reached the disk is exactly
/// the empty config the loader would treat as corrupt.
fn write_config(path: &std::path::Path, cfg: &Config) -> Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    let tmp = temp_sibling(path);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    sync_parent_dir(path);
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

/// Upper bound on the element count one legacy INI list may declare.
///
/// The old Java tool wrote small lists; a corrupt or hostile count must not size
/// an allocation (or drive a loop) from an untrusted file.
const MAX_IMPORTED_ENTRIES: usize = 1000;

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
        // Bounded: a corrupt/hostile count must not size an allocation from an
        // untrusted file (the loop would also spin for that many entries).
        let count: usize = p
            .get("INI_HIGILIGHT_COUNT")
            .and_then(|s| s.parse().ok())
            .map_or(0, |n: usize| n.min(MAX_IMPORTED_ENTRIES));
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
        let count: usize = p
            .get("CMD_COUNT")
            .and_then(|s| s.parse().ok())
            .map_or(0, |n: usize| n.min(MAX_IMPORTED_ENTRIES));
        if count > 0 {
            let mut cmds = Vec::with_capacity(count);
            for i in 0..count {
                if let Some(v) = p.get(&format!("CMD_{i}")) {
                    cmds.push(v.clone());
                }
            }
            if !cmds.is_empty() {
                // The old C++ LogFilter predates HarmonyOS, so its command list is
                // all adb commands.
                cfg.device.adb_commands = cmds;
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

    fn temp_config(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lf_cfg_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn backups(path: &std::path::Path) -> Vec<PathBuf> {
        let prefix = format!("{}.(", path.file_name().unwrap().to_string_lossy());
        let _ = prefix;
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|candidate| {
                let file = candidate
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                file.starts_with(&format!("{name}.")) && file.ends_with(".bak")
            })
            .collect()
    }

    #[test]
    fn a_corrupt_field_only_resets_its_own_section() {
        // One unusable value (here a string where a number belongs) used to reset
        // the theme, fonts, filters, colors and recent files along with it.
        let path = temp_config("section");
        std::fs::write(
            &path,
            "view = { font_size = 13.0, theme = \"dark\" }\nrecording = { cache_mib = \"nope\" }\n",
        )
        .unwrap();

        let ConfigFile::Recovered(cfg) = load_config_file(&path) else {
            panic!("a partially corrupt config must be recovered, not discarded");
        };
        assert_eq!(cfg.view.font_size, 13.0);
        assert_eq!(cfg.view.theme, "dark");
        // The broken section falls back to its default.
        assert_eq!(cfg.recording.cache_mib, 0);
        assert_eq!(cfg.recording.min_free_mib, 1024);
        assert_eq!(backups(&path).len(), 1, "the bad file must be backed up");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn repeated_corruption_never_overwrites_the_first_backup() {
        let path = temp_config("twice");
        for attempt in 0..2 {
            std::fs::write(
                &path,
                format!("view = {{ font_size = \"bad{attempt}\" }}\n"),
            )
            .unwrap();
            assert!(matches!(load_config_file(&path), ConfigFile::Recovered(_)));
        }
        assert_eq!(
            backups(&path).len(),
            2,
            "each failed load must keep its own backup"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A failed backup move must remove the reserved placeholder again: an
    /// empty `*.bak` file is exactly what a user would mistake for a copy of
    /// their settings, the opposite of what the backup exists to ensure. A
    /// non-empty directory cannot be renamed onto a file on any supported
    /// platform, so it is the deterministic stand-in for a failed rename.
    #[test]
    fn a_failed_backup_move_leaves_no_empty_placeholder() {
        let dir = tempdir_new();
        // The reserved placeholder, as `backup_path` would create it.
        let backup = dir.join("config.toml.1.1.bak");
        std::fs::write(&backup, b"").unwrap();
        let original = dir.join("config.toml");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(original.join("nested"), b"x").unwrap();

        assert!(
            move_to_backup(&original, &backup).is_err(),
            "renaming a non-empty directory onto a file must fail"
        );
        assert!(
            !backup.exists(),
            "a failed move must not leave the empty placeholder behind"
        );
        assert!(original.exists(), "the original must stay in place");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_config_is_distinct_from_an_unreadable_one() {
        let path = temp_config("missing");
        assert!(matches!(load_config_file(&path), ConfigFile::Missing));
        // A directory where the file belongs cannot be read; that must not look
        // like a fresh install (which would trigger the legacy INI migration).
        std::fs::create_dir_all(&path).unwrap();
        assert!(matches!(load_config_file(&path), ConfigFile::Unreadable));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn recordings_default_to_the_user_data_directory() {
        // Not the process CWD: a desktop-shortcut launch has a CWD that may not
        // be writable, and recordings must not scatter across directories.
        let dir = recordings_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "logs");
        assert_ne!(dir.parent().unwrap(), std::env::current_dir().unwrap());
        assert_eq!(
            dir,
            ProjectDirs::from("", "", "logfilter")
                .unwrap()
                .data_local_dir()
                .join("logs")
        );
    }

    #[test]
    fn ini_import_clamps_an_absurd_list_count() {
        // A corrupt/hostile legacy INI must not size an allocation (or drive a
        // loop) from an untrusted count; the import has to clamp it and keep the
        // entries it actually finds.
        let dir = std::env::temp_dir().join(format!("lf_ini_counts_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("LogFilter.ini"), "INI_WIDTH=800\n").unwrap();
        std::fs::write(
            dir.join("LogFilterColor.ini"),
            "INI_HIGILIGHT_COUNT=4000000000\nINI_HIGILIGHT_0=alpha\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("LogFilterCmd.ini"),
            "CMD_COUNT=4000000000\nCMD_0=logcat -v threadtime\n",
        )
        .unwrap();

        let cfg = import_from_ini(&dir).expect("legacy INI should import");
        assert_eq!(cfg.window.width, 800.0);
        assert_eq!(cfg.colors.highlights, vec!["alpha".to_string()]);
        assert_eq!(
            cfg.device.adb_commands,
            vec!["logcat -v threadtime".to_string()]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
        // An older saved config missing the hilog commands should get them added
        // to the hdc list.
        let mut cfg = Config::default();
        cfg.device.adb_commands = vec!["logcat -v threadtime".into()];
        cfg.device.hdc_commands = vec![];
        ensure_builtin_commands(&mut cfg);
        assert!(
            cfg.device.hdc_commands.iter().any(|c| c.contains("hilog")),
            "hilog command added to hdc list"
        );
        // Idempotent — a second call doesn't duplicate either list.
        let (na, nh) = (cfg.device.adb_commands.len(), cfg.device.hdc_commands.len());
        ensure_builtin_commands(&mut cfg);
        assert_eq!(
            (cfg.device.adb_commands.len(), cfg.device.hdc_commands.len()),
            (na, nh),
            "no duplicate on re-run"
        );
    }

    #[test]
    fn ensure_builtin_commands_keeps_kmsg_last() {
        // A saved config with the kernel-log command stranded before the other
        // adb builtins: it must be moved back to the end of the adb list.
        let mut cfg = Config::default();
        cfg.device.adb_commands =
            vec!["shell cat /proc/kmsg".into(), "logcat -v threadtime".into()];
        ensure_builtin_commands(&mut cfg);
        assert_eq!(
            cfg.device.adb_commands.last().map(String::as_str),
            Some("shell cat /proc/kmsg"),
            "kernel-log command stays last"
        );
    }

    #[test]
    fn transport_roundtrips_and_defaults_to_adb() {
        assert_eq!(DeviceConfig::default().transport, Transport::Adb);
        // A config without the transport field loads as adb.
        let old: Config = toml::from_str("[device]\n").unwrap();
        assert_eq!(old.device.transport, Transport::Adb);
        // Round-trips hdc + the hdc path.
        let mut cfg = Config::default();
        cfg.device.transport = Transport::Hdc;
        cfg.device.hdc_path = Some("/opt/harmony/hdc".into());
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.device.transport, Transport::Hdc);
        assert_eq!(back.device.hdc_path.as_deref(), Some("/opt/harmony/hdc"));
    }

    #[test]
    fn config_toml_roundtrips_new_persisted_fields() {
        let mut cfg = Config::default();
        cfg.view.columns_visible = [
            false, true, false, true, false, true, true, false, true, false,
        ];
        cfg.filters.find_on = Some(false);
        cfg.filters.highlight_on = Some(true);
        cfg.device.selected_cmd = "logcat -b radio -v time".into();
        cfg.device.selected_device = "emulator-5554".into();

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.view.columns_visible, cfg.view.columns_visible);
        assert_eq!(back.filters.find_on, Some(false));
        assert_eq!(back.filters.highlight_on, Some(true));
        assert_eq!(back.device.selected_cmd, "logcat -b radio -v time");
        assert_eq!(back.device.selected_device, "emulator-5554");
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
        assert_eq!(cfg.device.selected_cmd, "");
    }

    #[test]
    fn write_config_atomic_roundtrips_and_leaves_no_temp() {
        let dir = tempdir_new();
        let path = dir.join("rt_config.toml");
        let mut cfg = Config::default();
        cfg.device.selected_device = "dev1".into();
        cfg.view.columns_visible[6] = true; // show UID

        write_config(&path, &cfg).unwrap();
        assert!(path.exists(), "config file written");
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "temp file should be renamed away, not left behind"
        );
        let back = match load_config_file(&path) {
            ConfigFile::Loaded(cfg) => cfg,
            _ => panic!("a written config must parse cleanly"),
        };
        assert_eq!(back.device.selected_device, "dev1");
        assert!(back.view.columns_visible[6]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_config_file_is_backed_up_not_lost() {
        let dir = tempdir_new();
        let path = dir.join("bad_config.toml");
        std::fs::write(&path, "this is = not valid toml ][").unwrap();

        // Nothing is recoverable from this one, so it becomes defaults — but the
        // original must survive in a timestamped backup.
        assert!(matches!(load_config_file(&path), ConfigFile::Recovered(_)));
        let saved = backups(&path);
        assert_eq!(
            saved.len(),
            1,
            "corrupt config must be preserved: {saved:?}"
        );
        assert!(std::fs::read_to_string(&saved[0])
            .unwrap()
            .contains("not valid toml"));
        // The bad content is moved aside, but a parseable config must take its
        // place: leaving no config behind would make the next launch treat this
        // as a fresh install and run the legacy INI import.
        assert!(path.exists(), "the recovered config is written back");
        assert!(
            matches!(load_config_file(&path), ConfigFile::Loaded(_)),
            "the written-back config must parse cleanly"
        );
        assert_eq!(
            backups(&path).len(),
            1,
            "re-reading a valid config must not create another backup"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recovered_config_keeps_readable_sections_on_disk() {
        let dir = tempdir_new();
        let path = dir.join("partial_config.toml");
        // Syntactically valid TOML, but `adb_commands` has the wrong type, so
        // only `[device]` is discarded. (A *syntax* error anywhere makes
        // `recover_config` bail to defaults wholesale — see line 377.)
        std::fs::write(
            &path,
            "[view]\nfont_size = 16.0\ntheme = \"dark\"\n\n[device]\nadb_commands = 42\n",
        )
        .unwrap();

        let recovered = match load_config_file(&path) {
            ConfigFile::Recovered(cfg) => cfg,
            other => panic!(
                "expected Recovered, got {:?}",
                std::mem::discriminant(&other)
            ),
        };
        assert_eq!(recovered.view.font_size, 16.0);
        assert_eq!(recovered.view.theme, "dark");

        // Those sections must survive a restart without any further save.
        let reloaded = match load_config_file(&path) {
            ConfigFile::Loaded(cfg) => cfg,
            _ => panic!("the written-back config must parse cleanly"),
        };
        assert_eq!(reloaded.view.font_size, 16.0);
        assert_eq!(reloaded.view.theme, "dark");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sanitize_view_clamps_values_that_reach_the_renderer() {
        let defaults = ViewConfig::default();

        // A font size large enough to trip epaint's texture-atlas assertion.
        let mut view = ViewConfig {
            font_size: 5000.0,
            ..defaults.clone()
        };
        sanitize_view(&mut view);
        assert_eq!(view.font_size, MAX_FONT_SIZE);

        // Zero/negative would make the table unreadable with no way back.
        for bad in [0.0, -12.0] {
            let mut view = ViewConfig {
                font_size: bad,
                ..defaults.clone()
            };
            sanitize_view(&mut view);
            assert_eq!(view.font_size, MIN_FONT_SIZE, "font_size {bad} clamped");
        }

        // Non-finite cannot be clamped, so it falls back to the default.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut view = ViewConfig {
                font_size: bad,
                ..defaults.clone()
            };
            sanitize_view(&mut view);
            assert_eq!(view.font_size, defaults.font_size);
        }

        // Columns: clamped when finite, replaced per-column when not.
        let mut view = defaults.clone();
        view.columns[0] = -5.0;
        view.columns[1] = 1.0e9;
        view.columns[2] = f32::NAN;
        sanitize_view(&mut view);
        assert_eq!(view.columns[0], 0.0);
        assert_eq!(view.columns[1], MAX_COLUMN_WIDTH);
        assert_eq!(view.columns[2], defaults.columns[2]);

        // A valid config must pass through untouched.
        let mut view = defaults.clone();
        sanitize_view(&mut view);
        assert_eq!(view.font_size, defaults.font_size);
        assert_eq!(view.columns, defaults.columns);
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
