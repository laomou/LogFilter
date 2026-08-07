use crate::config::{self, parse_color, Config};
use crate::filter::FilterSpec;
use crate::fonts::{bump_global_text_sizes, install_ui_font, list_user_font_stems};
use crate::io::{read_appended, send_decoded_lines, send_utf8_lines, Tail};
use crate::lock::{MutexExt, RwLockExt};
use crate::model::{EncodingChoice, LevelMask, LogFormat, Model};
use crate::parser::parse_line_hinted;
use crate::transport::{self, Transport};
use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};
use egui_extras::{Column, TableBuilder};
use egui_i18n::tr;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

pub struct App {
    pub cfg: Config,
    pub model: Arc<RwLock<Model>>,
    pub shared_filter: Arc<RwLock<FilterSpec>>,
    pub gen: Arc<AtomicU64>,
    /// Monotonic id of the current line source (file load or device session). Each
    /// new load/adb-run bumps it; the ingest thread drops any queued line whose
    /// epoch != this, so a superseded load can't interleave into the new one.
    pub source_epoch: Arc<AtomicU64>,
    /// Expected format for the current source; written before epoch bump so the
    /// ingest thread can skip redundant regex attempts on homogeneous streams.
    source_format_hint: Arc<Mutex<LogFormat>>,
    pub wake: Arc<(Mutex<bool>, Condvar)>,
    pub status: String,
    /// Last `status` value the auto-expiry logic observed, and the egui time it
    /// first appeared — used to clear transient messages after a few seconds so
    /// they don't linger or permanently hide the "Selected N" readout.
    last_status_seen: String,
    status_shown_at: f64,
    pub ui: UiState,

    pub selected_rows: HashSet<usize>,
    /// Fixed end of a range selection (set on plain click / single-row moves).
    pub selection_anchor: Option<usize>,
    /// Moving end of a range selection — the row that Shift+Arrow / Shift+Click
    /// extends toward. Distinct from the anchor so that repeatedly pressing
    /// Shift+↓ always moves the leading edge forward, not a random HashSet element.
    pub selection_cursor: Option<usize>,
    pub pending_scroll: Option<usize>,
    pub visible_table_rows: usize,
    /// Window inner size captured each frame, saved on exit.
    last_window_size: Option<egui::Vec2>,

    // All font stems found in config/fonts — just metadata, no bytes loaded.
    // Populated once at startup via list_user_font_stems().
    pub user_font_stems: Vec<(String, String)>,

    // device session (adb / hdc)
    pub line_tx: Sender<(u64, String)>,
    pub session: Option<transport::Session>,
    pub devices: Vec<String>,
    pub selected_device: String,
    pub selected_cmd: String,
    /// Device-connector backend (adb / hdc). Chosen in the toolbar, persisted.
    pub transport: Transport,
    pub auto_scroll: bool,
    /// Result channel for an in-flight device-list probe (`adb devices` /
    /// `hdc list targets`). Keeping the probe off the UI thread prevents startup
    /// and the refresh button from freezing the window when the connector is
    /// slow or unavailable.
    device_refresh_rx: Option<Receiver<Result<Vec<String>, String>>>,

    /// egui context, kept so background loaders/tail pollers can request a
    /// repaint and hand reload requests back to the UI thread.
    ctx: egui::Context,
    /// Set by the tail poller when the followed file is rotated/truncated (or
    /// grows under a reload-only encoding). The UI thread picks it up and does a
    /// full `open_file` reload. `None` = nothing pending.
    reload_request: Arc<Mutex<Option<PathBuf>>>,
    /// Encoding actually used for the current file load, resolved by the loader
    /// (notably "Local" → the sniffed UTF-8/legacy codepage). Shown in the status
    /// bar so the user sees the real encoding, not just their menu choice.
    /// `None` until the initial read finishes (or when the source is a device session).
    detected_encoding: Arc<Mutex<Option<String>>>,

    // Per-frame caches: recomputed lazily when source data changes.
    cached_highlight_palette: Vec<Color32>,
    /// Raw palette values used to build `cached_highlight_palette`. Keeping this
    /// alongside the parsed colors lets us invalidate on a color-value change,
    /// not merely when the number of configured colors changes.
    cached_highlight_palette_raw: Vec<String>,
    cached_highlight_tokens: Vec<String>,
    cached_find_tokens: Vec<String>,
    /// Raw highlight/find strings that were used to produce the token caches above.
    cached_highlight_raw: String,
    cached_find_raw: String,
    /// Pre-parsed level colors (V/D/I/W/E/F) to avoid per-cell string parsing.
    cached_level_colors: [Color32; 6],
    /// Picker panel option cache: column type last cached for.
    cached_picker_col: Option<PickerCol>,
    /// Picker panel option cache: pre-built sorted option list.
    cached_picker_options: Vec<(String, usize)>,
    /// entries.len() at the time picker options were cached — used as invalidation key.
    cached_picker_entries_len: usize,
    /// Cached shortcut-rows for the empty-table view. Invalidated on language switch.
    cached_shortcut_rows: Vec<EmptyShortcutRow>,
    /// Column widths mirrored from the table each frame; persisted to config on exit.
    cached_col_widths: [f32; 10],
    /// Pending result from a background save_filtered() operation.
    save_result_rx:
        Option<crossbeam_channel::Receiver<Result<(usize, std::path::PathBuf), String>>>,
    /// Set by the background file-load thread when a mid-stream read error
    /// truncates a load, so the UI can tell the user the file loaded only
    /// partially instead of silently presenting it as complete.
    load_error: Arc<Mutex<Option<String>>>,
}

pub struct UiState {
    // Text filters (bottom search bar)
    pub find: String,
    pub find_on: bool,
    pub remove: String,
    pub remove_on: bool,
    pub highlight: String,
    pub highlight_on: bool,

    // Column-picker filter state. None = 通过所有值；Some(set) = 只保留 set 里的。
    pub allowed_pids: Option<std::collections::HashSet<String>>,
    pub allowed_tids: Option<std::collections::HashSet<String>>,
    pub allowed_tags: Option<std::collections::HashSet<String>>,
    /// Tags explicitly excluded via Alt+right-click — applied even when
    /// `allowed_tags` is None so newly streamed tags are also blocked.
    pub disallowed_tags: std::collections::HashSet<String>,
    pub allowed_levels: Option<LevelMask>,

    // Encoding (set via Encoding menu)
    pub encoding: String,

    // Column visibility (View → Columns / right-click column header)
    pub col_bookmark: bool,
    pub col_line: bool,
    pub col_date: bool,
    pub col_time: bool,
    pub col_loglv: bool,
    pub col_pid: bool,
    pub col_thread: bool,
    pub col_uid: bool,
    pub col_tag: bool,
    pub col_message: bool,

    pub goto_line: String,

    // Quick-filter toggles
    pub bookmarks_only: bool,

    // Picker panel state (open only when Some).
    pub picker: Option<PickerState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerCol {
    Level,
    Pid,
    Tid,
    Tag,
}

#[derive(Debug, Clone)]
pub struct PickerState {
    pub col: PickerCol,
    pub search: String,
    pub anchor: egui::Pos2,
    /// True on the frame the picker is first shown. The same click that opened
    /// it (a header click or a context-menu item) lands outside the freshly
    /// created panel, so without this guard the "click outside → close" check
    /// would close it the instant it opens.
    pub just_opened: bool,
}

impl UiState {
    /// Number of currently-visible table columns. Used to forbid hiding the last
    /// one, which would leave an empty, unusable table.
    fn visible_column_count(&self) -> usize {
        [
            self.col_bookmark,
            self.col_line,
            self.col_date,
            self.col_time,
            self.col_loglv,
            self.col_pid,
            self.col_thread,
            self.col_uid,
            self.col_tag,
            self.col_message,
        ]
        .into_iter()
        .filter(|&v| v)
        .count()
    }

    fn from_config(cfg: &Config) -> Self {
        // Columns-visible order matches ViewConfig::columns:
        // line, date, time, level, pid, thread, uid, tag, bookmark, message.
        let cv = cfg.view.columns_visible;
        Self {
            find: cfg.filters.find.clone(),
            find_on: cfg.filters.find_on.unwrap_or(!cfg.filters.find.is_empty()),
            remove: cfg.filters.remove.clone(),
            remove_on: cfg
                .filters
                .remove_on
                .unwrap_or(!cfg.filters.remove.is_empty()),
            highlight: cfg.filters.highlight.clone(),
            highlight_on: cfg
                .filters
                .highlight_on
                .unwrap_or(!cfg.filters.highlight.is_empty()),
            allowed_pids: None,
            allowed_tids: None,
            allowed_tags: None,
            disallowed_tags: std::collections::HashSet::new(),
            allowed_levels: None,
            encoding: cfg.view.encoding.clone(),
            col_line: cv[0],
            col_date: cv[1],
            col_time: cv[2],
            col_loglv: cv[3],
            col_pid: cv[4],
            col_thread: cv[5],
            col_uid: cv[6],
            col_tag: cv[7],
            col_bookmark: cv[8],
            col_message: cv[9],
            goto_line: String::new(),
            bookmarks_only: false,
            picker: None,
        }
    }

    fn to_filter_spec(&self) -> FilterSpec {
        FilterSpec {
            allowed_levels: self.allowed_levels,
            allowed_pids: self.allowed_pids.clone(),
            allowed_tids: self.allowed_tids.clone(),
            allowed_tags: self.allowed_tags.clone(),
            disallowed_tags: self.disallowed_tags.clone(),
            find: if self.find_on {
                FilterSpec::tokens(&self.find)
            } else {
                vec![]
            },
            remove: if self.remove_on {
                FilterSpec::tokens(&self.remove)
            } else {
                vec![]
            },
            bookmarks_only: self.bookmarks_only,
        }
    }

    fn write_back(&self, cfg: &mut Config) {
        cfg.filters.find = self.find.clone();
        cfg.filters.remove = self.remove.clone();
        cfg.filters.highlight = self.highlight.clone();
        cfg.filters.find_on = Some(self.find_on);
        cfg.filters.remove_on = Some(self.remove_on);
        cfg.filters.highlight_on = Some(self.highlight_on);
        cfg.view.encoding = self.encoding.clone();
        // Same order as ViewConfig::columns / columns_visible.
        cfg.view.columns_visible = [
            self.col_line,
            self.col_date,
            self.col_time,
            self.col_loglv,
            self.col_pid,
            self.col_thread,
            self.col_uid,
            self.col_tag,
            self.col_bookmark,
            self.col_message,
        ];
    }
}

impl App {
    /// Production constructor. `main` loads the config once so it can apply
    /// persisted native-window settings before handing the same config to App.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        initial_file: Option<PathBuf>,
    ) -> Self {
        let mut app = Self::from_ctx(&cc.egui_ctx, cfg, initial_file);
        // Pre-populate the device combo on startup so the user doesn't have to
        // click ↻ once before they can pick a device. Skipped in the test-only
        // constructor since it shells out to the device connector.
        app.refresh_devices(cc.egui_ctx.clone());
        app
    }

    /// Core constructor shared by the production `new` and the test harness.
    /// Takes an `egui::Context` (not the eframe `CreationContext`) and an
    /// already-loaded `Config` so tests can inject a default config without
    /// touching the user's real config file or spawning a device probe.
    fn from_ctx(ctx: &egui::Context, cfg: Config, initial_file: Option<PathBuf>) -> Self {
        apply_theme(ctx, &cfg.view.theme);
        tune_table_visuals(ctx);
        init_i18n();
        // Apply the stored language (or auto-detect) at startup.
        resolve_startup_lang(&cfg.view.lang);
        let font_stems = list_user_font_stems();
        install_ui_font(ctx, &cfg.view.font, &font_stems);
        bump_global_text_sizes(ctx);
        // egui defaults Ctrl+= / Ctrl+- / Ctrl+0 to changing the global zoom_factor,
        // which scales the entire UI (menus, toolbar, table). We only want those
        // shortcuts to change the table font size, so disable egui's handler and
        // implement our own in `update()`.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let ui = UiState::from_config(&cfg);
        let shared_filter = Arc::new(RwLock::new(ui.to_filter_spec()));
        // Bounded so a fast file reader can't buffer the whole file as queued
        // Strings ahead of the (slower) parse/append step — it blocks instead,
        // capping peak memory. 8192 ≈ a few ingest batches of headroom.
        let (line_tx, line_rx) = bounded::<(u64, String)>(8192);
        // Restore the last-used command/device; fall back to the first command
        // of the saved backend / "(any)" device when not previously saved.
        let transport = cfg.device.transport;
        let selected_cmd = if !cfg.device.selected_cmd.is_empty() {
            cfg.device.selected_cmd.clone()
        } else {
            cfg.device
                .commands(transport)
                .first()
                .cloned()
                .unwrap_or_else(|| "logcat -v threadtime".into())
        };
        let selected_device = cfg.device.selected_device.clone();
        // Prime caches from the initial config so the first frame doesn't reallocate.
        let init_hl_raw = if ui.highlight_on {
            ui.highlight.clone()
        } else {
            String::new()
        };
        let init_find_raw = if ui.find_on {
            ui.find.clone()
        } else {
            String::new()
        };
        let init_palette: Vec<Color32> = cfg
            .colors
            .highlights
            .iter()
            .map(|s| parse_color(s))
            .collect();
        let init_palette_raw = cfg.colors.highlights.clone();
        let init_level_colors = parse_level_colors(&cfg);
        let init_col_widths = cfg.view.columns;
        let mut app = Self {
            cfg,
            model: Arc::new(RwLock::new(Model::default())),
            shared_filter,
            gen: Arc::new(AtomicU64::new(0)),
            source_epoch: Arc::new(AtomicU64::new(0)),
            source_format_hint: Arc::new(Mutex::new(LogFormat::Unknown)),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            status: String::new(),
            last_status_seen: String::new(),
            status_shown_at: 0.0,
            ui,
            selected_rows: HashSet::new(),
            selection_anchor: None,
            selection_cursor: None,
            pending_scroll: None,
            visible_table_rows: 1,
            last_window_size: None,
            user_font_stems: font_stems,
            line_tx,
            session: None,
            devices: Vec::new(),
            selected_device,
            selected_cmd,
            transport,
            auto_scroll: true,
            device_refresh_rx: None,
            ctx: ctx.clone(),
            reload_request: Arc::new(Mutex::new(None)),
            detected_encoding: Arc::new(Mutex::new(None)),
            cached_highlight_palette: init_palette,
            cached_highlight_palette_raw: init_palette_raw,
            cached_highlight_tokens: if init_hl_raw.is_empty() {
                vec![]
            } else {
                FilterSpec::tokens(&init_hl_raw)
            },
            cached_find_tokens: if init_find_raw.is_empty() {
                vec![]
            } else {
                FilterSpec::tokens(&init_find_raw)
            },
            cached_highlight_raw: init_hl_raw,
            cached_find_raw: init_find_raw,
            cached_level_colors: init_level_colors,
            cached_picker_col: None,
            cached_picker_options: Vec::new(),
            cached_picker_entries_len: 0,
            cached_shortcut_rows: empty_shortcut_rows(),
            cached_col_widths: init_col_widths,
            save_result_rx: None,
            load_error: Arc::new(Mutex::new(None)),
        };
        app.spawn_filter_thread(ctx.clone());
        app.spawn_ingest_thread(ctx.clone(), line_rx);
        if let Some(path) = initial_file {
            if let Err(e) = app.open_file(&path) {
                app.status =
                    tr!("status_failed_open", { e: &format!("{}: {}", path.display(), e) });
            }
        }
        app.notify_filter();
        app
    }

    /// Test-only constructor: builds an `App` on the given egui context with a
    /// default `Config`, skipping config-file I/O and the device probe so
    /// UI tests run hermetically.
    #[cfg(test)]
    pub fn new_for_test(ctx: &egui::Context) -> Self {
        Self::from_ctx(ctx, Config::default(), None)
    }

    pub fn open_file(&mut self, path: &Path) -> Result<()> {
        // Open the file here to validate access and hold the handle through the
        // background load — avoids TOCTOU (open-to-use race) and eliminates the
        // duplicate open in the reader thread.
        let file = std::fs::File::open(path)?;

        // Stop any device session so its lines don't interleave with the file.
        self.stop_session();

        // Claim a fresh source epoch *before* clearing so any lines still queued
        // from a previous load/adb are dropped by the ingest thread. Write the
        // format hint first so the ingest thread never reads a stale hint for
        // this epoch.
        *self.source_format_hint.lock_recover() = LogFormat::Unknown;
        let epoch = self.source_epoch.fetch_add(1, Ordering::AcqRel) + 1;

        // Whether this is a switch to a *different* file, as opposed to an
        // automatic reload of the same file after a truncation/rotation. Only a
        // genuine source change should drop the column-picker filters — reloading
        // the same file (or re-opening it) keeps them, since the values (PID/tag/…)
        // still refer to the same data. Captured before `model.clear()` overwrites
        // the stored path below.
        let source_changed = self.model.read_recover().file_path.as_deref() != Some(path);

        // Reset the model synchronously (cheap) and let lines stream in.
        {
            let mut model = self.model.write_recover();
            model.clear();
            model.file_path = Some(path.to_path_buf());
        }
        self.selected_rows.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        if source_changed {
            self.reset_column_filters();
        }
        config::add_recent(&mut self.cfg, path);
        self.notify_filter();

        // Background reader: read + decode, then feed lines through the existing
        // ingest channel so parsing/appending/repaint happen incrementally on
        // the ingest thread — the UI stays responsive for large files. The
        // reader bails out early if a newer load supersedes this epoch. After the
        // initial read it keeps the thread alive to follow (tail) appends.
        let tx = self.line_tx.clone();
        let source_epoch = self.source_epoch.clone();
        let choice = self.encoding_choice();
        let load_error = self.load_error.clone();
        let reload_request = self.reload_request.clone();
        let detected_encoding = self.detected_encoding.clone();
        let ctx = self.ctx.clone();
        let follow_path = path.to_path_buf();
        let src = path.display().to_string();
        // Clear any error from a previous load before starting this one.
        *self.load_error.lock_recover() = None;
        // Clear the previous file's detected encoding until this load resolves it.
        *self.detected_encoding.lock_recover() = None;
        thread::Builder::new()
            .name("file-load".into())
            .spawn(move || {
                let res = match choice {
                    EncodingChoice::Utf8 => {
                        send_utf8_lines(file, tx.clone(), epoch, source_epoch.clone())
                    }
                    EncodingChoice::Local => {
                        send_decoded_lines(file, tx.clone(), epoch, source_epoch.clone(), choice)
                    }
                };
                let tail = match res {
                    Ok(t) => t,
                    Err(e) => {
                        // A mid-stream read error truncated the load; record it so
                        // the UI surfaces "partially loaded" rather than silence.
                        if let Ok(mut slot) = load_error.lock() {
                            *slot = Some(format!("{src}: {e}"));
                        }
                        return;
                    }
                };
                // Publish the encoding actually used (resolves "Local" to the
                // sniffed codepage) so the status bar can show it.
                *detected_encoding.lock_recover() = Some(tail.encoding_name().to_string());
                ctx.request_repaint();
                follow_file(
                    &follow_path,
                    tail,
                    &tx,
                    epoch,
                    &source_epoch,
                    &reload_request,
                    &ctx,
                );
            })?;
        Ok(())
    }

    fn encoding_choice(&self) -> EncodingChoice {
        match self.ui.encoding.as_str() {
            "local" => EncodingChoice::Local,
            _ => EncodingChoice::Utf8,
        }
    }

    fn notify_filter(&mut self) {
        // A filter-spec change rebuilds `filtered` (asynchronously, on the filter
        // thread), so the current selection positions — which index into the OLD
        // `filtered` — become meaningless. Clear them rather than let stale/out-of
        // -range indices repoint at unrelated rows (wrong Ctrl+C / F2 target) or
        // inflate the "Selected N" status count.
        self.selected_rows.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        *self.shared_filter.write_recover() = self.ui.to_filter_spec();
        self.refilter();
    }

    /// Bump the filter generation and wake the filter thread to recompute
    /// `filtered` from scratch, WITHOUT clearing the selection or rewriting the
    /// shared spec. Used when the underlying data a filter depends on changed
    /// but the spec itself didn't — e.g. a bookmark toggled while the
    /// "bookmarks only" filter is active.
    fn refilter(&self) {
        self.gen.fetch_add(1, Ordering::AcqRel);
        let (lock, cvar) = &*self.wake;
        *lock.lock_recover() = true;
        cvar.notify_one();
    }

    /// Resolve and apply the UI language from a stored config value
    /// ("auto"/"en"/"zh"). "auto" detects from the system locale.
    fn set_lang(&mut self, stored: &str) {
        self.cfg.view.lang = stored.into();
        let code = match stored {
            "zh" => "zh-CN",
            "en" => "en-US",
            _ => {
                let loc = sys_locale::get_locale()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if loc.starts_with("zh") {
                    "zh-CN"
                } else {
                    "en-US"
                }
            }
        };
        egui_i18n::set_language(code);
        // Rebuild: shortcut-row strings embed translated labels.
        self.cached_shortcut_rows = empty_shortcut_rows();
    }

    fn switch_theme(&mut self, ctx: &egui::Context, theme: &str) {
        let old_defaults = if self.cfg.view.theme == "dark" {
            config::ColorsConfig::dark_defaults()
        } else {
            config::ColorsConfig::light_defaults()
        };
        let new_defaults = if theme == "dark" {
            config::ColorsConfig::dark_defaults()
        } else {
            config::ColorsConfig::light_defaults()
        };
        self.cfg.colors.migrate(&old_defaults, &new_defaults);
        self.cached_level_colors = parse_level_colors(&self.cfg);
        self.cached_highlight_palette = self
            .cfg
            .colors
            .highlights
            .iter()
            .map(|s| parse_color(s))
            .collect();
        self.cached_highlight_palette_raw = self.cfg.colors.highlights.clone();
        self.cfg.view.theme = theme.into();
        apply_theme(ctx, theme);
    }
    fn spawn_ingest_thread(&self, ctx: egui::Context, rx: Receiver<(u64, String)>) {
        let model = self.model.clone();
        let wake = self.wake.clone();
        let source_epoch = self.source_epoch.clone();
        let source_format_hint = self.source_format_hint.clone();
        thread::Builder::new()
            .name("ingest".into())
            .spawn(move || {
                let mut batch: Vec<(u64, String)> = Vec::with_capacity(256);
                // Cached hint to avoid locking on every line; refreshed when the
                // epoch changes. Locked to the first non-Unknown format seen so
                // we skip the full 4-regex scan for homogeneous streams.
                let mut hint = LogFormat::Unknown;
                let mut hint_epoch: u64 = u64::MAX;
                loop {
                    // block for first line
                    let Ok(first) = rx.recv() else {
                        return;
                    };
                    batch.clear();
                    batch.push(first);
                    // drain more if available (up to 512 lines / 25ms)
                    let deadline = std::time::Instant::now() + Duration::from_millis(25);
                    while batch.len() < 512 {
                        let remain = deadline.saturating_duration_since(std::time::Instant::now());
                        if remain.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(remain) {
                            Ok(l) => batch.push(l),
                            Err(_) => break,
                        }
                    }
                    // Drop lines from a superseded source (an older file load / adb
                    // session) so they never interleave into the current one.
                    let cur = source_epoch.load(Ordering::Acquire);
                    // Refresh hint on epoch change (new file or adb command).
                    if cur != hint_epoch {
                        hint = *source_format_hint.lock_recover();
                        hint_epoch = cur;
                    }
                    let mut appended = false;
                    {
                        let mut m = model.write_recover();
                        for (ep, line) in batch.drain(..) {
                            if ep != cur {
                                continue;
                            }
                            let (entry, fmt) = parse_line_hinted(line, hint);
                            // Lock in the format on the first matched line so subsequent
                            // lines skip the full scan entirely.
                            if hint == LogFormat::Unknown && fmt != LogFormat::Unknown {
                                hint = fmt;
                            }
                            m.append(entry);
                            appended = true;
                        }
                    }
                    if appended {
                        // Wake the filter thread for an append-only pass. Deliberately
                        // do NOT bump `gen` — that's reserved for filter-spec changes,
                        // which force a full recompute (see spawn_filter_thread).
                        let (lock, cvar) = &*wake;
                        *lock.lock_recover() = true;
                        cvar.notify_one();
                        ctx.request_repaint();
                    }
                }
            })
            .expect("spawn ingest thread");
    }
}

/// Poll a followed file every 500 ms and stream appended lines through `tx`,
/// keeping the current epoch. Runs on the file-load thread after the initial
/// read. Exits when the epoch changes (a new file/adb load superseded this one)
/// or the receiver is gone. On rotation/truncation (or growth under a
/// reload-only encoding like UTF-16) it records a reload request and exits;
/// the UI thread performs the actual `open_file` reload.
#[allow(clippy::too_many_arguments)]
fn follow_file(
    path: &Path,
    tail: Tail,
    tx: &Sender<(u64, String)>,
    epoch: u64,
    source_epoch: &Arc<AtomicU64>,
    reload_request: &Arc<Mutex<Option<PathBuf>>>,
    ctx: &egui::Context,
) {
    const POLL: Duration = Duration::from_millis(500);
    let request_reload = || {
        *reload_request.lock_recover() = Some(path.to_path_buf());
        ctx.request_repaint();
    };
    match tail {
        Tail::Append { mut offset, enc } => loop {
            thread::sleep(POLL);
            if source_epoch.load(Ordering::Acquire) != epoch {
                return; // superseded by a newer load
            }
            match read_appended(path, offset, enc, tx, epoch) {
                Ok(a) if a.truncated => {
                    request_reload();
                    return;
                }
                Ok(a) => {
                    if a.offset != offset {
                        offset = a.offset;
                        ctx.request_repaint();
                    }
                }
                // Transient read error (e.g. file briefly missing during
                // rotation): keep polling rather than giving up.
                Err(_) => {}
            }
        },
    }
}

/// The log format a command produces. Built-in commands carry it explicitly;
/// for anything else, fall back to the `-v <fmt>` flag (`/kmsg` → kernel).
fn detect_format_from_cmd(cmd: &str) -> LogFormat {
    if let Some(b) = transport::builtin_command(cmd) {
        return b.format;
    }
    if cmd.contains("/kmsg") {
        return LogFormat::Kernel;
    }
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    for w in parts.windows(2) {
        if w[0] == "-v" {
            return match w[1] {
                "threadtime" | "long" => LogFormat::ThreadTime,
                "time" => LogFormat::Time,
                "brief" | "process" | "tag" => LogFormat::Brief,
                _ => LogFormat::Unknown,
            };
        }
    }
    LogFormat::Unknown
}

impl App {
    /// Binary path override for the current transport (adb_path / hdc_path).
    fn transport_override(&self) -> Option<String> {
        self.cfg
            .device
            .binary_override(self.transport)
            .map(str::to_string)
    }

    /// Start (or restart) a streaming session for the selected transport, device,
    /// and command, replacing any session or file tail currently feeding the view.
    fn run_session(&mut self) {
        self.stop_session();
        // Write the format hint before bumping the epoch so the ingest thread
        // never races between seeing the new epoch and reading a stale hint.
        *self.source_format_hint.lock_recover() = detect_format_from_cmd(&self.selected_cmd);
        let epoch = self.source_epoch.fetch_add(1, Ordering::AcqRel) + 1;

        // A fresh run starts from an empty table — clear any entries left from a
        // previous run/file (mirrors the file-load path). The epoch bump above
        // already ensures stale queued lines are dropped by the ingest thread.
        {
            let mut model = self.model.write_recover();
            model.clear();
        }
        self.selected_rows.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        // Keep the column-picker filters (level/PID/TID/tag): Run/Restart
        // re-monitor the same device/app, so the values stay relevant — unlike a
        // file open, which switches to an unrelated source.
        self.notify_filter();

        let device = if self.selected_device.is_empty() {
            None
        } else {
            // The list may label a device "SERIAL (unauthorized)"; -s needs the serial.
            Some(transport::device_serial(&self.selected_device))
        };
        match transport::Session::start(
            self.transport,
            self.transport_override().as_deref(),
            device,
            &self.selected_cmd,
            self.line_tx.clone(),
            epoch,
        ) {
            Ok(s) => {
                self.session = Some(s);
                self.status = tr!("status_dev_started", { tool: self.transport.binary(), cmd: &self.selected_cmd });
            }
            Err(e) => {
                self.status = tr!("status_dev_start_failed", { tool: self.transport.binary(), e: &format!("{}", e) });
            }
        }
    }

    fn stop_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.stop();
            self.status = tr!("status_dev_stopped", { tool: self.transport.binary() });
        }
    }

    fn toggle_pause(&mut self) {
        if let Some(s) = &self.session {
            let new = !s.is_paused();
            s.set_paused(new);
            self.status = if new {
                tr!("status_dev_paused", { tool: self.transport.binary() })
            } else {
                tr!("status_dev_resumed", { tool: self.transport.binary() })
            };
        }
    }

    fn clear(&mut self) {
        // Bump the epoch so a running file-tail (or any queued lines) stops
        // feeding this cleared view — otherwise the tail poller would keep
        // appending new lines into a table the user just emptied.
        //
        // Exception: while a device session is live, keep the epoch — the session
        // bakes it into every line it emits, so bumping here would make all of
        // its *future* lines be dropped by the ingest thread, silently killing
        // the stream. Clearing during capture should empty the view yet keep new
        // lines flowing. (run_session supersedes any file-tail, so none is active.)
        if self.session.is_none() {
            self.source_epoch.fetch_add(1, Ordering::AcqRel);
        }
        {
            let mut m = self.model.write_recover();
            m.clear();
        }
        self.selected_rows.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.notify_filter();
    }

    /// Reset the value-based column-picker filters (PID/TID/tag/level). These
    /// hold literal string/level values captured from one source; carrying them
    /// into a *different* file silently filters the new data by unrelated values
    /// (often leaving a blank table with no visible cause). Text filters
    /// (find/remove/highlight) are content patterns and are kept.
    ///
    /// Only genuine source changes reset: opening a different file does, but
    /// reloading the same file (truncation/rotation) and adb Run/Restart do not —
    /// those keep watching the same data, so the values stay relevant.
    fn reset_column_filters(&mut self) {
        self.ui.allowed_pids = None;
        self.ui.allowed_tids = None;
        self.ui.allowed_tags = None;
        self.ui.disallowed_tags.clear();
        self.ui.allowed_levels = None;
    }

    /// Tab-separated text of an entry's *visible* columns, in display order, so a
    /// copied row matches what the table shows. Notably this omits a hidden column
    /// (e.g. the UID column, off by default) instead of emitting an empty field —
    /// otherwise a normal log copies as `…tid\t\ttag…` with a phantom column.
    fn visible_row_text(&self, e: &crate::model::LogEntry) -> String {
        let mut fields: Vec<String> = Vec::with_capacity(9);
        if self.ui.col_line {
            fields.push(e.line_no.to_string());
        }
        if self.ui.col_date {
            fields.push(e.date().to_string());
        }
        if self.ui.col_time {
            fields.push(e.time().to_string());
        }
        if self.ui.col_loglv {
            fields.push(e.level.as_char().to_string());
        }
        if self.ui.col_pid {
            fields.push(e.pid().to_string());
        }
        if self.ui.col_thread {
            fields.push(e.tid().to_string());
        }
        if self.ui.col_uid {
            fields.push(e.uid().to_string());
        }
        if self.ui.col_tag {
            fields.push(e.tag().to_string());
        }
        if self.ui.col_message {
            fields.push(e.message().to_string());
        }
        fields.join("\t")
    }

    fn copy_selected_rows_text(&self) -> String {
        let m = self.model.read_recover();
        let mut rows: Vec<&usize> = self.selected_rows.iter().collect();
        rows.sort();
        let texts: Vec<String> = rows
            .iter()
            .filter_map(|&&r| {
                let &ei = m.filtered.get(r)?;
                Some(self.visible_row_text(&m.entries[ei as usize]))
            })
            .collect();
        texts.join("\n")
    }

    /// Copy a single column from all selected rows, one line per row.
    fn copy_selected_column_text(
        entries: &[crate::model::LogEntry],
        filtered: &[u32],
        selected_rows: &HashSet<usize>,
        col: fn(&crate::model::LogEntry) -> &str,
    ) -> String {
        let mut rows: Vec<&usize> = selected_rows.iter().collect();
        rows.sort();
        let texts: Vec<&str> = rows
            .iter()
            .filter_map(|&&r| {
                let &ei = filtered.get(r)?;
                Some(col(&entries[ei as usize]))
            })
            .collect();
        texts.join("\n")
    }

    fn copy_selected_row(&mut self) {
        if self.selected_rows.is_empty() {
            return;
        }
        let text = self.copy_selected_rows_text();
        let n = text.lines().count();
        self.copy_text_to_clipboard(&text, n);
    }

    /// Copy `text` to the system clipboard and set the status bar accordingly.
    fn copy_text_to_clipboard(&mut self, text: &str, n: usize) {
        match arboard::Clipboard::new() {
            Ok(mut c) => {
                if let Err(e) = c.set_text(text) {
                    self.status = tr!("status_clipboard_error", { e: &format!("{e}") });
                } else {
                    self.status = if n > 1 {
                        tr!("status_copied_rows", { n: &n.to_string() })
                    } else {
                        tr!("status_copied_row")
                    };
                }
            }
            Err(e) => self.status = tr!("status_clipboard_error", { e: &format!("{e}") }),
        }
    }

    fn save_filtered(&mut self) {
        let m = self.model.read_recover();
        if m.filtered.is_empty() && m.entries.is_empty() {
            self.status = tr!("status_nothing_to_save");
            return;
        }
        let default_name = format!(
            "logfilter_{}.txt",
            chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
        );
        let path = rfd::FileDialog::new()
            .set_file_name(default_name)
            .save_file();
        let Some(dest) = path else { return };

        // Snapshot the filtered entries so the background thread doesn't need
        // to hold the model lock while writing (which could block the UI). We
        // save each entry's original line verbatim so the output preserves the
        // source format (adb stream / original file) rather than a re-joined
        // table.
        let rows: Vec<String> = m
            .filtered
            .iter()
            .map(|&ei| m.entries[ei as usize].raw().to_string())
            .collect();
        drop(m);

        let n = rows.len();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.save_result_rx = Some(rx);
        thread::Builder::new()
            .name("save-filtered".into())
            .spawn(move || {
                let res = (|| -> Result<(), std::io::Error> {
                    use std::io::{BufWriter, Write};
                    let f = std::fs::File::create(&dest)?;
                    let mut w = BufWriter::new(f);
                    for line in &rows {
                        writeln!(w, "{}", line)?;
                    }
                    w.flush()?;
                    Ok(())
                })();
                let result = match res {
                    Ok(()) => Ok((n, dest)),
                    Err(e) => Err(format!("{e}")),
                };
                let _ = tx.send(result);
            })
            .expect("spawn save thread");
    }

    /// Alt+left-click on a Tag cell → "only this tag".
    fn add_show_tag(&mut self, tag: &str) {
        if tag.is_empty() {
            return;
        }
        let mut set = std::collections::HashSet::new();
        set.insert(tag.to_string());
        self.ui.allowed_tags = Some(set);
        self.notify_filter();
    }

    /// Alt+right-click on a Tag cell → exclude this tag.
    fn add_remove_tag(&mut self, tag: &str) {
        if tag.is_empty() {
            return;
        }
        // Add to the blacklist; also remove from allowed_tags if it's an explicit
        // allowlist so the two sets stay consistent.
        self.ui.disallowed_tags.insert(tag.to_string());
        if let Some(ref mut set) = self.ui.allowed_tags {
            set.remove(tag);
        }
        self.notify_filter();
    }

    /// Begin an asynchronous `adb devices` probe unless one is already active.
    fn refresh_devices(&mut self, ctx: egui::Context) {
        if self.device_refresh_rx.is_some() {
            return;
        }
        let tsp = self.transport;
        let override_path = self.transport_override();
        let (tx, rx) = bounded(1);
        self.device_refresh_rx = Some(rx);
        if let Err(e) = thread::Builder::new()
            .name("device-list".into())
            .spawn(move || {
                let result = transport::list_devices(tsp, override_path.as_deref())
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
                ctx.request_repaint();
            })
        {
            self.device_refresh_rx = None;
            self.status = tr!("status_dev_devices_failed", { tool: self.transport.binary(), e: &format!("{e}") });
        }
    }

    /// Apply a completed device probe without disturbing a still-valid explicit
    /// selection. Called only from the UI thread.
    fn apply_devices_result(&mut self, result: Result<Vec<String>, String>) {
        match result {
            Ok(list) => {
                let n = list.len();
                self.devices = list;
                // Preserve current selection if still present; otherwise pick
                // first device but never overwrite an explicit user choice
                // unless it disappeared.
                if !self.selected_device.is_empty()
                    && !self.devices.iter().any(|d| d == &self.selected_device)
                {
                    self.selected_device = String::new();
                }
                self.status = if n == 0 {
                    tr!("status_dev_devices_zero")
                } else {
                    tr!("status_dev_devices", { tool: self.transport.binary(), n: n })
                };
            }
            Err(e) => {
                self.devices.clear();
                self.status =
                    tr!("status_dev_devices_failed", { tool: self.transport.binary(), e: &e });
            }
        }
    }

    /// Poll the background device probe, if any. A disconnected channel means
    /// the worker failed before reporting, so clear the busy state and surface
    /// a diagnostic rather than leaving the refresh control disabled forever.
    fn poll_device_refresh(&mut self) {
        let Some(rx) = &self.device_refresh_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.device_refresh_rx = None;
                self.apply_devices_result(result);
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.device_refresh_rx = None;
                self.status = tr!("status_dev_devices_failed", {
                    tool: self.transport.binary(),
                    e: &"device probe terminated unexpectedly".to_string()
                });
            }
        }
    }

    fn spawn_filter_thread(&self, ctx: egui::Context) {
        let model = self.model.clone();
        let spec_lock = self.shared_filter.clone();
        let gen = self.gen.clone();
        let wake = self.wake.clone();
        thread::Builder::new()
            .name("filter".into())
            .spawn(move || {
                let (lock, cvar) = &*wake;
                // Incremental state carried across wakes:
                //  * `last_spec_gen` — the `gen` the current `filtered` was built for.
                //  * `processed_len` — how many entries are already reflected in it.
                // A wake does a full recompute only when the spec changed or the log
                // shrank (clear/reload); otherwise it just filters the appended tail
                // and extends `filtered`, so streaming stays O(N) overall instead of
                // O(N²) (which is what re-scanning from 0 every batch would cost).
                let mut last_spec_gen: u64 = u64::MAX;
                let mut processed_len: usize = 0;
                loop {
                    let mut pending = lock.lock_recover();
                    while !*pending {
                        // Recover the guard on poisoning rather than crashing the
                        // filter thread if another thread panicked while holding it.
                        let (p, _) = cvar
                            .wait_timeout(pending, Duration::from_secs(60))
                            .unwrap_or_else(|e| e.into_inner());
                        pending = p;
                    }
                    *pending = false;
                    drop(pending);

                    let spec_gen = gen.load(Ordering::Acquire);
                    let spec = spec_lock.read_recover().clone();
                    let entries_len = model.read_recover().entries.len();

                    let full = spec_gen != last_spec_gen || entries_len < processed_len;
                    let start = if full { 0 } else { processed_len };

                    let cap = if full {
                        entries_len / 4
                    } else {
                        (entries_len - start) / 2 + 1
                    };
                    let mut out: Vec<u32> = Vec::with_capacity(cap);
                    let mut aborted = false;
                    // Process in chunks holding the read lock once per chunk instead
                    // of once per row: amortizes lock cost while still yielding to
                    // writers (ingest/clear) and checking abort between chunks.
                    const CHUNK: usize = 4096;
                    let mut i = start;
                    while i < entries_len {
                        // Abort only on a *spec* change; data growth is picked up by
                        // the next wake continuing from `processed_len`.
                        if gen.load(Ordering::Acquire) != spec_gen {
                            aborted = true;
                            break;
                        }
                        let end = (i + CHUNK).min(entries_len);
                        let m = model.read_recover();
                        let hi = end.min(m.entries.len());
                        for j in i..hi {
                            if spec.matches(&m.entries[j], j as u32, &m.bookmarks) {
                                out.push(j as u32);
                            }
                        }
                        drop(m);
                        if hi < end {
                            aborted = true; // entries shrank (cleared) — stop early
                            break;
                        }
                        i = end;
                    }

                    if aborted {
                        // Discard the partial result and force a full redo next wake.
                        processed_len = 0;
                        last_spec_gen = u64::MAX;
                        continue;
                    }

                    // Commit under the write lock, re-validating against a clear that
                    // could have landed since we snapshotted `entries_len` — otherwise
                    // `filtered` could hold indices past the end of a shrunk log.
                    let mut m = model.write_recover();
                    if gen.load(Ordering::Acquire) != spec_gen || m.entries.len() < entries_len {
                        drop(m);
                        processed_len = 0;
                        last_spec_gen = u64::MAX;
                        continue;
                    }
                    if full {
                        m.filtered = out;
                    } else {
                        m.filtered.extend(out);
                    }
                    drop(m);
                    processed_len = entries_len;
                    last_spec_gen = spec_gen;
                    ctx.request_repaint();
                }
            })
            .expect("spawn filter thread");
    }

    fn toggle_bookmark(&mut self, entry_idx: u32) {
        {
            let mut m = self.model.write_recover();
            if m.bookmarks.contains(&entry_idx) {
                m.bookmarks.remove(&entry_idx);
            } else {
                m.bookmarks.insert(entry_idx);
            }
        }
        // When the "bookmarks only" filter is active, the set of matching rows
        // just changed, so the filtered view must be recomputed — otherwise an
        // unbookmarked row lingers (or a newly bookmarked one stays hidden).
        if self.ui.bookmarks_only {
            self.refilter();
        }
    }

    fn toggle_selected_bookmark(&mut self) {
        let entries: Vec<u32> = {
            let m = self.model.read_recover();
            self.selected_rows
                .iter()
                .filter_map(|&r| m.filtered.get(r).copied())
                .collect()
        };
        for entry_idx in entries {
            self.toggle_bookmark(entry_idx);
        }
    }

    fn select_filtered_row_with_len(&mut self, row: usize, len: usize) {
        if let Some(row) = clamp_filtered_row(row, len) {
            self.selected_rows.clear();
            self.selected_rows.insert(row);
            self.pending_scroll = Some(row);
            self.selection_anchor = Some(row);
            self.selection_cursor = Some(row);
        }
    }

    fn page_selected_row(&mut self, forward: bool) {
        let len = self.model.read_recover().filtered.len();
        // Page from the deterministic cursor (like the arrow keys), not an
        // arbitrary HashSet element — otherwise a multi-row selection pages from
        // a random member and the jump distance is non-deterministic.
        let cursor = self.selection_cursor.or(self.selection_anchor);
        let Some(row) = page_row(cursor, len, self.visible_table_rows, forward) else {
            return;
        };
        self.select_filtered_row_with_len(row, len);
    }

    /// Move selection by `delta` rows (±1) and update the selection anchor.
    fn move_selected_row(&mut self, delta: isize) {
        let m = self.model.read_recover();
        let len = m.filtered.len();
        if len == 0 {
            return;
        }
        drop(m);
        let new = match self.selection_cursor {
            // No current row yet: the first Arrow (either direction) lands on row
            // 0 — otherwise Down would skip past it to row 1.
            None => 0,
            Some(cur) => {
                let cur = cur.min(len - 1);
                if delta < 0 {
                    cur.saturating_sub(1)
                } else {
                    (cur + 1).min(len - 1)
                }
            }
        };
        self.selected_rows.clear();
        self.selected_rows.insert(new);
        self.pending_scroll = Some(new);
        self.selection_anchor = Some(new);
        self.selection_cursor = Some(new);
    }

    /// Extend the selection range from `selection_anchor` by `delta` rows (±1).
    fn extend_selection(&mut self, delta: isize) {
        let m = self.model.read_recover();
        let len = m.filtered.len();
        if len == 0 {
            return;
        }
        let anchor = self
            .selection_anchor
            .unwrap_or(0)
            .min(len.saturating_sub(1));
        // Use the explicit cursor (moving end) rather than an arbitrary HashSet
        // element — HashSet iteration order is non-deterministic.
        let cur = self
            .selection_cursor
            .unwrap_or(anchor)
            .min(len.saturating_sub(1));
        drop(m);
        let new = if delta < 0 {
            cur.saturating_sub(1)
        } else {
            (cur + 1).min(len.saturating_sub(1))
        };
        let (lo, hi) = if new < anchor {
            (new, anchor)
        } else {
            (anchor, new)
        };
        self.selected_rows.clear();
        for i in lo..=hi {
            self.selected_rows.insert(i);
        }
        self.pending_scroll = Some(new);
        self.selection_cursor = Some(new);
    }

    fn adjust_table_font_size(&mut self, delta: f32) {
        self.cfg.view.font_size = adjusted_table_font_size(self.cfg.view.font_size, delta);
    }

    fn reset_table_font_size(&mut self) {
        self.cfg.view.font_size = Config::default().view.font_size;
    }

    /// Recompute caches for highlight palette & tokens only when source data changed.
    fn refresh_highlight_caches(&mut self) {
        // Palette changes are rare, but comparing the raw configuration is
        // necessary: a user may replace one color with another while keeping
        // the same number of palette entries.
        let palette_raw = &self.cfg.colors.highlights;
        if self.cached_highlight_palette_raw != *palette_raw {
            self.cached_highlight_palette = palette_raw.iter().map(|s| parse_color(s)).collect();
            self.cached_highlight_palette_raw = palette_raw.clone();
        }

        // Token caches: only invalidate when the raw filter text changes.
        let hl_raw = if self.ui.highlight_on {
            self.ui.highlight.as_str()
        } else {
            ""
        };
        let f_raw = if self.ui.find_on {
            self.ui.find.as_str()
        } else {
            ""
        };
        if self.cached_highlight_raw.as_str() != hl_raw {
            self.cached_highlight_tokens = if hl_raw.is_empty() {
                vec![]
            } else {
                FilterSpec::tokens(hl_raw)
            };
            self.cached_highlight_raw = hl_raw.to_string();
        }
        if self.cached_find_raw.as_str() != f_raw {
            self.cached_find_tokens = if f_raw.is_empty() {
                vec![]
            } else {
                FilterSpec::tokens(f_raw)
            };
            self.cached_find_raw = f_raw.to_string();
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};

        let cmd = Modifiers::COMMAND;
        let shortcut = |key| KeyboardShortcut::new(cmd, key);

        // egui/winit deliver Ctrl/Cmd+C as a semantic egui::Event::Copy, not a
        // Key::C press — so matching a Key::C keyboard shortcut never fires on the
        // real app (notably on Windows). Detect the Copy event instead.
        let copy_event = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));

        // When a text field is focused, let it keep its own editing shortcuts
        // (copy/cut/paste, arrows, etc.); row/table shortcuts below don't fire.
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        // No field focused: Ctrl/Cmd+C copies the selected log row(s). (Clicking a
        // row clears field focus, so the common "select row → copy" path lands
        // here.)
        if copy_event {
            if !self.selected_rows.is_empty() {
                self.copy_selected_row();
            }
            return;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&shortcut(Key::S))) {
            self.save_filtered();
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&shortcut(Key::Equals))) {
            self.adjust_table_font_size(1.0);
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&shortcut(Key::Minus))) {
            self.adjust_table_font_size(-1.0);
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&shortcut(Key::Num0))) {
            self.reset_table_font_size();
            return;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&shortcut(Key::F2))) {
            self.toggle_selected_bookmark();
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F2)) {
            self.jump_bookmark(false);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F3)) {
            self.jump_bookmark(true);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::PageUp)) {
            self.page_selected_row(false);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::PageDown)) {
            self.page_selected_row(true);
            return;
        }
        // ArrowUp/ArrowDown: move selection by 1 row; Shift extends the range.
        // Two separate consume_key calls are needed — consume_key does an EXACT
        // modifier match, so NONE only fires without Shift and SHIFT only fires
        // with Shift. Reading the modifier first then consuming with NONE is wrong
        // because the consume always returns false when Shift is actually held.
        {
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::ArrowUp)) {
                self.extend_selection(-1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.move_selected_row(-1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::ArrowDown)) {
                self.extend_selection(1);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.move_selected_row(1);
            }
        }
    }

    fn jump_bookmark(&mut self, forward: bool) {
        let m = self.model.read_recover();
        if m.filtered.is_empty() {
            return;
        }
        let cur = self
            .selection_cursor
            .or(self.selection_anchor)
            .unwrap_or(0)
            .min(m.filtered.len().saturating_sub(1));
        let indices: Vec<usize> = (0..m.filtered.len())
            .filter(|&i| m.bookmarks.contains(&m.filtered[i]))
            .collect();
        if indices.is_empty() {
            return;
        }
        let next = if forward {
            indices
                .iter()
                .find(|&&i| i > cur)
                .copied()
                .or_else(|| indices.first().copied())
        } else {
            indices
                .iter()
                .rev()
                .find(|&&i| i < cur)
                .copied()
                .or_else(|| indices.last().copied())
        };
        if let Some(n) = next {
            self.selected_rows.clear();
            self.selected_rows.insert(n);
            self.pending_scroll = Some(n);
            self.selection_anchor = Some(n);
            self.selection_cursor = Some(n);
        }
    }

    /// Build a sorted Vec of (label, count) pairs for a picker column.
    /// Called only when the cached list is stale, not per frame.
    fn build_sorted_options(model: &Model, col: PickerCol) -> Vec<(String, usize)> {
        let mut v = match col {
            PickerCol::Pid => model
                .pid_counts
                .iter()
                .map(|(k, &c)| (k.clone(), c))
                .collect::<Vec<_>>(),
            PickerCol::Tid => model
                .tid_counts
                .iter()
                .map(|(k, &c)| (k.clone(), c))
                .collect(),
            PickerCol::Tag => model
                .tag_counts
                .iter()
                .map(|(k, &c)| (k.clone(), c))
                .collect(),
            PickerCol::Level => {
                let labels = ['V', 'D', 'I', 'W', 'E', 'F'];
                labels
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| model.level_counts[i] > 0)
                    .map(|(i, &lb)| (lb.to_string(), model.level_counts[i]))
                    .collect()
            }
        };
        match col {
            PickerCol::Level => {}
            _ => v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0))),
        }
        v
    }

    fn render_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = self.ui.picker.clone() else {
            return;
        };

        // Build/reuse cached option list (sorted by count desc, then key).
        // The sorted list invalidates only when the picker column changes or new
        // entries were appended (entries.len() differs). This avoids a full
        // clone+sort per frame while the picker is open.
        {
            let m = self.model.read_recover();
            let entries_len = m.entries.len();
            if self.cached_picker_col != Some(picker.col)
                || self.cached_picker_entries_len != entries_len
            {
                self.cached_picker_options = Self::build_sorted_options(&m, picker.col);
                self.cached_picker_col = Some(picker.col);
                self.cached_picker_entries_len = entries_len;
            }
        }
        let options: Vec<(String, usize)> = self.cached_picker_options.clone();

        let (title, current_selected) =
            {
                match picker.col {
                    PickerCol::Pid => {
                        let sel =
                            self.ui.allowed_pids.clone().unwrap_or_else(|| {
                                options.iter().map(|(k, _)| k.clone()).collect()
                            });
                        (tr!("filter_pid"), sel)
                    }
                    PickerCol::Tid => {
                        let sel =
                            self.ui.allowed_tids.clone().unwrap_or_else(|| {
                                options.iter().map(|(k, _)| k.clone()).collect()
                            });
                        (tr!("filter_thread"), sel)
                    }
                    PickerCol::Tag => {
                        let sel =
                            self.ui.allowed_tags.clone().unwrap_or_else(|| {
                                options.iter().map(|(k, _)| k.clone()).collect()
                            });
                        (tr!("filter_tag"), sel)
                    }
                    PickerCol::Level => {
                        let masks = crate::model::LEVEL_MASKS;
                        let labels = ['V', 'D', 'I', 'W', 'E', 'F'];
                        let current_mask = self.ui.allowed_levels.unwrap_or(LevelMask::ALL);
                        let sel: std::collections::HashSet<String> = (0..6)
                            .filter(|&i| current_mask.contains(masks[i]))
                            .map(|i| labels[i].to_string())
                            .collect();
                        (tr!("filter_lv"), sel)
                    }
                }
            };

        // Draw the popup as a floating Area.
        let mut selected = current_selected;
        let mut search = picker.search.clone();
        let mut close = false;
        let mut changed = false;

        let area_resp = egui::Area::new(egui::Id::new("column_picker"))
            .order(egui::Order::Foreground)
            .fixed_pos(picker.anchor + egui::vec2(0.0, 4.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(240.0);
                    ui.set_max_width(320.0);
                    ui.horizontal(|ui| {
                        ui.strong(title);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").on_hover_text(tr!("close")).clicked() {
                                close = true;
                            }
                        });
                    });
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label("🔍");
                        ui.add(
                            egui::TextEdit::singleline(&mut search)
                                .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                                .desired_width(f32::INFINITY),
                        );
                    });

                    ui.horizontal(|ui| {
                        if ui.small_button(tr!("select_all")).clicked() {
                            selected = options.iter().map(|(k, _)| k.clone()).collect();
                            changed = true;
                        }
                        if ui.small_button(tr!("clear")).clicked() {
                            selected.clear();
                            changed = true;
                        }
                        if ui
                            .small_button(tr!("reset"))
                            .on_hover_text(tr!("reset_hover"))
                            .clicked()
                        {
                            match picker.col {
                                PickerCol::Pid => self.ui.allowed_pids = None,
                                PickerCol::Tid => self.ui.allowed_tids = None,
                                PickerCol::Tag => {
                                    self.ui.allowed_tags = None;
                                    self.ui.disallowed_tags.clear();
                                }
                                PickerCol::Level => self.ui.allowed_levels = None,
                            }
                            close = true;
                            self.notify_filter();
                        }
                    });
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            let search_lower = search.to_lowercase();
                            for (val, cnt) in &options {
                                if !search_lower.is_empty()
                                    && !val.to_lowercase().contains(&search_lower)
                                {
                                    continue;
                                }
                                let mut on = selected.contains(val);
                                let label = format!("{val}    ({cnt})");
                                if ui.checkbox(&mut on, label).changed() {
                                    if on {
                                        selected.insert(val.clone());
                                    } else {
                                        selected.remove(val);
                                    }
                                    changed = true;
                                }
                            }
                        });
                });
            });

        // Persist picker search text; clear the just-opened guard after the
        // first frame so subsequent outside-clicks close the panel normally.
        if let Some(p) = self.ui.picker.as_mut() {
            p.search = search;
            p.just_opened = false;
        }

        // If user clicked outside the picker, close it — but not on the very
        // frame it opened (the opening click itself lands outside the panel).
        let clicked_outside = !picker.just_opened
            && ctx.input(|i| i.pointer.any_click())
            && !area_resp
                .response
                .rect
                .contains(ctx.input(|i| i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO)));
        if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }

        // Apply changes to filter state.
        if changed {
            match picker.col {
                PickerCol::Pid => self.ui.allowed_pids = Some(selected.clone()),
                PickerCol::Tid => self.ui.allowed_tids = Some(selected.clone()),
                PickerCol::Tag => {
                    // Keep disallowed_tags in sync: anything visible in the picker
                    // but not selected is explicitly excluded.
                    self.ui.disallowed_tags = options
                        .iter()
                        .map(|(k, _)| k.clone())
                        .filter(|k| !selected.contains(k))
                        .collect();
                    self.ui.allowed_tags = Some(selected.clone());
                }
                PickerCol::Level => {
                    let masks = crate::model::LEVEL_MASKS;
                    let labels = ['V', 'D', 'I', 'W', 'E', 'F'];
                    let mut mask = LevelMask::empty();
                    for (i, lb) in labels.iter().enumerate() {
                        if selected.contains(&lb.to_string()) {
                            mask |= masks[i];
                        }
                    }
                    self.ui.allowed_levels = Some(mask);
                }
            }
            self.notify_filter();
        }

        if close {
            self.ui.picker = None;
        }
    }
}

fn apply_theme(ctx: &egui::Context, theme: &str) {
    if theme == "dark" {
        ctx.set_theme(egui::Theme::Dark);
    } else {
        ctx.set_theme(egui::Theme::Light);
    }
}

fn tune_table_visuals(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.visuals.widgets.hovered.expansion = 0.0;
        style.interaction.selectable_labels = false;
    });
}

fn init_i18n() {
    let en = include_str!("../assets/i18n/en-US.egl");
    let zh = include_str!("../assets/i18n/zh-CN.egl");
    egui_i18n::set_fallback("en-US");
    egui_i18n::load_translations_from_text("en-US", en).unwrap();
    egui_i18n::load_translations_from_text("zh-CN", zh).unwrap();
}

fn resolve_startup_lang(stored: &str) {
    let code = match stored {
        "zh" => "zh-CN",
        "en" => "en-US",
        _ => {
            let loc = sys_locale::get_locale()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if loc.starts_with("zh") {
                "zh-CN"
            } else {
                "en-US"
            }
        }
    };
    egui_i18n::set_language(code);
}

fn open_dir(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener).arg(path).spawn();
}

fn fit_middle(ui: &egui::Ui, s: &str, max_width: f32) -> String {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let width = |t: &str| -> f32 {
        ui.painter()
            .layout_no_wrap(t.to_string(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    };
    if width(s) <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let join = |keep: usize| -> String {
        let head_len = keep.div_ceil(2);
        let tail_len = keep - head_len;
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        format!("{head}…{tail}")
    };
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

fn level_color(lv: LevelMask, colors: &[Color32; 6]) -> Color32 {
    let idx = crate::model::level_index(lv).unwrap_or(0);
    colors[idx]
}

/// Parse the 6 level colors from Config into an array (once at startup).
fn parse_level_colors(cfg: &Config) -> [Color32; 6] {
    let masks = crate::model::LEVEL_MASKS;
    let strings: [&str; 6] = [
        &cfg.colors.level_v,
        &cfg.colors.level_d,
        &cfg.colors.level_i,
        &cfg.colors.level_w,
        &cfg.colors.level_e,
        &cfg.colors.level_f,
    ];
    let mut colors = [Color32::BLACK; 6];
    for (i, s) in strings.iter().enumerate() {
        colors[i] = parse_color(s);
    }
    // Keep unused binding for future-proofing — ensure masks aligns with strings.
    let _ = masks;
    colors
}

const EMPTY_SHORTCUT_TOP_PADDING_ROWS: usize = 3;

struct EmptyShortcutRow {
    tag: String,
    message: String,
}

fn empty_shortcut_rows() -> Vec<EmptyShortcutRow> {
    let rows = [
        (
            tr!("shortcut_file"),
            format!("Ctrl/Cmd+S - {}", tr!("sh_save")),
        ),
        (
            tr!("shortcut_bookmarks"),
            format!("Ctrl/Cmd+F2 - {}", tr!("sh_toggle_bookmark")),
        ),
        (
            tr!("shortcut_bookmarks"),
            format!("F2 - {}", tr!("sh_prev_bookmark")),
        ),
        (
            tr!("shortcut_bookmarks"),
            format!("F3 - {}", tr!("sh_next_bookmark")),
        ),
        (
            tr!("shortcut_line"),
            format!("Ctrl/Cmd+C - {}", tr!("sh_copy_selected")),
        ),
        (
            tr!("shortcut_line"),
            format!("PageUp / PageDown - {}", tr!("sh_page_up_down")),
        ),
        (
            tr!("shortcut_line"),
            format!(
                "{} - {}",
                tr!("sh_double_click_lbl"),
                tr!("sh_double_click")
            ),
        ),
        (
            tr!("shortcut_line"),
            format!("{} - {}", tr!("sh_alt_click_lbl"), tr!("sh_alt_click")),
        ),
        (
            tr!("shortcut_font"),
            format!("Ctrl/Cmd+Plus / Ctrl/Cmd+Minus - {}", tr!("sh_font_size")),
        ),
        (
            tr!("shortcut_font"),
            format!("Ctrl/Cmd+0 - {}", tr!("sh_reset_font")),
        ),
    ];
    rows.into_iter()
        .map(|(tag, message)| EmptyShortcutRow { tag, message })
        .collect()
}

fn adjusted_table_font_size(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(13.0, 18.0)
}

fn clamp_filtered_row(row: usize, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(row.min(len - 1))
    }
}

fn page_row(
    selected: Option<usize>,
    len: usize,
    visible_rows: usize,
    forward: bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = selected.unwrap_or(0).min(len - 1);
    let page = visible_rows.max(1);
    Some(if forward {
        current.saturating_add(page).min(len - 1)
    } else {
        current.saturating_sub(page)
    })
}

/// Build a LayoutJob rendering `text` with highlight tokens as background spans
/// and find tokens as thin-underlined spans. All tokens are matched case-insensitively.
fn build_highlighted(
    text: &str,
    highlights: &[String],
    finds: &[String],
    fg: Color32,
    font: FontId,
    highlight_palette: &[Color32],
) -> LayoutJob {
    let mut job = LayoutJob::default();
    if text.is_empty() {
        return job;
    }
    // Fast path: no tokens → plain text, skip to_lowercase allocation.
    if highlights.is_empty() && finds.is_empty() {
        job.append(
            text,
            0.0,
            TextFormat {
                color: fg,
                font_id: font,
                ..Default::default()
            },
        );
        return job;
    }

    // Collect matches: (start, end, kind) where kind=Some(hi_index) = highlight, None = find-underline
    let mut hits: Vec<(usize, usize, Option<usize>)> = Vec::new();

    // Fast(er) path: when all tokens and text are pure ASCII, use byte-level
    // case folding with to_ascii_lowercase — zero allocation, unlike
    // text.to_lowercase() which allocates a new String every call.
    let all_ascii = text.is_ascii()
        && highlights.iter().all(|t| t.is_ascii())
        && finds.iter().all(|t| t.is_ascii());
    if all_ascii {
        let hay = text.as_bytes();
        for (ti, tok) in highlights.iter().enumerate() {
            if tok.is_empty() {
                continue;
            }
            let nee = tok.as_bytes();
            if nee.len() > hay.len() {
                continue;
            }
            'outer: for start in 0..=hay.len() - nee.len() {
                for (j, &nb) in nee.iter().enumerate() {
                    if hay[start + j].to_ascii_lowercase() != nb {
                        continue 'outer;
                    }
                }
                hits.push((start, start + nee.len(), Some(ti)));
            }
        }
        for tok in finds {
            if tok.is_empty() {
                continue;
            }
            let nee = tok.as_bytes();
            if nee.len() > hay.len() {
                continue;
            }
            'outer: for start in 0..=hay.len() - nee.len() {
                for (j, &nb) in nee.iter().enumerate() {
                    if hay[start + j].to_ascii_lowercase() != nb {
                        continue 'outer;
                    }
                }
                hits.push((start, start + nee.len(), None));
            }
        }
    } else {
        // Fallback: Unicode-correct case-insensitive search.
        // We search the *original* `text` char-by-char so that all byte offsets
        // we record are valid indices into `text` — unlike searching a
        // `text.to_lowercase()` copy, which can change byte lengths for
        // characters such as ẞ (3 bytes) → ß (2 bytes), making offsets from the
        // lowercased copy invalid when applied back to `text`.
        let all_tokens: Vec<(Option<usize>, &str)> = highlights
            .iter()
            .enumerate()
            .map(|(i, t)| (Some(i), t.as_str()))
            .chain(finds.iter().map(|t| (None, t.as_str())))
            .collect();

        for (kind, tok) in &all_tokens {
            if tok.is_empty() {
                continue;
            }
            // Collect the token's chars lowercased once.
            let tok_chars: Vec<char> = tok.chars().collect();
            let tok_char_count = tok_chars.len();
            // Slide a window of tok_char_count chars over text.
            // Use char_indices so we always have valid byte positions.
            let char_positions: Vec<(usize, char)> = text.char_indices().collect();
            let n = char_positions.len();
            if tok_char_count > n {
                continue;
            }
            let mut i = 0;
            while i + tok_char_count <= n {
                let matches = char_positions[i..i + tok_char_count]
                    .iter()
                    .zip(tok_chars.iter())
                    .all(|((_, tc), nc)| tc.to_lowercase().eq(nc.to_lowercase()));
                if matches {
                    let s = char_positions[i].0;
                    let e = if i + tok_char_count < n {
                        char_positions[i + tok_char_count].0
                    } else {
                        text.len()
                    };
                    hits.push((s, e, *kind));
                    // Advance past this match; always move at least 1 char.
                    i += tok_char_count.max(1);
                } else {
                    i += 1;
                }
            }
        }
    }

    hits.sort_by_key(|h| (h.0, h.1));

    // Merge overlaps: keep earliest-start, longest span; later hits inside get dropped.
    let mut merged: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for h in hits {
        if let Some(last) = merged.last_mut() {
            if h.0 < last.1 {
                if h.1 > last.1 {
                    last.1 = h.1;
                }
                continue;
            }
        }
        merged.push(h);
    }

    let base = TextFormat {
        color: fg,
        font_id: font.clone(),
        ..Default::default()
    };
    let mut cursor = 0;
    for (s, e, kind) in merged {
        if s > cursor {
            job.append(&text[cursor..s], 0.0, base.clone());
        }
        let mut fmt = base.clone();
        match kind {
            Some(hi) if !highlight_palette.is_empty() => {
                let bg = highlight_palette[hi % highlight_palette.len()];
                fmt.background = bg;
                let lum = bg.r() as u32 * 299 + bg.g() as u32 * 587 + bg.b() as u32 * 114;
                fmt.color = if lum > 128_000 {
                    Color32::BLACK
                } else {
                    Color32::WHITE
                };
            }
            // Find matches get a thin underline to mark the matched substring.
            _ => {
                fmt.underline = egui::Stroke::new(0.5, fg);
            }
        }
        job.append(&text[s..e], 0.0, fmt);
        cursor = e;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base);
    }
    job
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Capture window inner size each frame so we can persist it on exit.
        self.last_window_size = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());

        // Poll background save result.
        if let Some(rx) = &self.save_result_rx {
            match rx.try_recv() {
                Ok(Ok((n, path))) => {
                    self.status = tr!("status_saved", { n: &n.to_string(), path: &path.display().to_string() });
                    self.save_result_rx = None;
                }
                Ok(Err(e)) => {
                    self.status = tr!("status_save_failed", { e: &e });
                    self.save_result_rx = None;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {} // still in progress
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // The save thread dropped its sender without sending a result
                    // (it panicked). Surface a failure rather than spin forever
                    // treating it as "in progress".
                    self.status = tr!("status_save_failed", { e: &"save thread terminated unexpectedly".to_string() });
                    self.save_result_rx = None;
                }
            }
        }

        self.poll_device_refresh();

        // A followed file was rotated/truncated: reload it from scratch. Taken
        // before other work so the fresh epoch is in place for this frame.
        let reload = self.reload_request.lock_recover().take();
        if let Some(path) = reload {
            // Only reload if it's still the file we're viewing (guards against a
            // stale request landing after the user opened something else).
            let still_current = self
                .model
                .read_recover()
                .file_path
                .as_ref()
                .is_some_and(|p| p == &path);
            if still_current {
                if let Err(e) = self.open_file(&path) {
                    self.status = tr!("status_failed_open", { e: &format!("{}", e) });
                }
            }
        }

        // Surface a truncated file load (mid-stream read error) once.
        if let Some(msg) = self.load_error.lock_recover().take() {
            self.status = tr!("status_load_truncated", { e: &msg });
        }

        // Detect a device session that ended on its own (device unplugged, adb
        // exited, bad command) so the UI stops showing it as live and surfaces
        // any captured stderr instead of a silently frozen stream.
        if self.session.as_ref().is_some_and(|s| s.has_ended()) {
            // The session ended on its own (stdout closed). Reap it first — that
            // joins the stderr worker so the captured failure reason is complete,
            // instead of racing the stdout/stderr close and losing it.
            let mut s = self.session.take().expect("checked is_some above");
            s.reap();
            let err = s.stderr_text();
            self.status = if err.is_empty() {
                tr!("status_dev_ended", { tool: self.transport.binary() })
            } else {
                tr!("status_dev_ended_err", { tool: self.transport.binary(), e: &err })
            };
        }

        self.handle_shortcuts(&ctx);
        self.ui_menu_bar(ui);
        self.ui_options_panel(ui);
        self.ui_status_bar(ui);
        self.ui_indicator(ui);
        self.ui_table(ui);
        // Column picker popup (Excel-style) — an Area, shown on the context.
        self.render_picker(&ctx);

        // Drag-drop
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(first) = dropped.into_iter().find_map(|f| f.path) {
            if let Err(e) = self.open_file(&first) {
                self.status = tr!("status_failed_open_dropped", { e: &format!("{}", e) });
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.ui.write_back(&mut self.cfg);
        if let Some(size) = self.last_window_size {
            self.cfg.window.width = size.x;
            self.cfg.window.height = size.y;
        }
        self.cfg.view.columns = self.cached_col_widths;
        // Persist the last-used adb command/device so the next launch restores it.
        self.cfg.device.selected_cmd = self.selected_cmd.clone();
        self.cfg.device.selected_device = self.selected_device.clone();
        self.cfg.device.transport = self.transport;
        // On exit there's no UI left to surface a status, but a failed save
        // silently loses the user's window size / filters / column widths /
        // recent files. Log it so it's at least diagnosable rather than invisible.
        if let Err(e) = config::save(&self.cfg) {
            eprintln!("logfilter: failed to save config on exit: {e}");
        }
    }
}

impl App {
    fn ui_menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Menu bar — File · Format · View · Encoding
        let mut recent_open: Option<PathBuf> = None;
        // Set when the Encoding menu changes value; handled after the menu closure
        // to re-decode the currently open file (can't call open_file mid-borrow).
        let mut encoding_changed = false;
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button(tr!("m_file"), |ui| {
                    if ui.button(tr!("open")).clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            if let Err(e) = self.open_file(&path) {
                                self.status = tr!("status_failed_open", { e: &format!("{}", e) });
                            }
                        }
                        ui.close();
                    }
                    ui.menu_button(tr!("recent"), |ui| {
                        // Prune entries whose file no longer exists so the list
                        // doesn't accumulate dead links.
                        config::prune_missing_recent(&mut self.cfg);
                        let recent = self.cfg.recent.files.clone();
                        if recent.is_empty() {
                            ui.label(tr!("recent_empty"));
                        }
                        for p in recent {
                            if ui.button(p.display().to_string()).clicked() {
                                recent_open = Some(p);
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button(tr!("save_filtered")).clicked() {
                        self.save_filtered();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(tr!("exit")).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button(tr!("m_format"), |ui| {
                    // ── Font submenu: lists imported fonts ────────────────
                    ui.menu_button(tr!("font"), |ui| {
                        ui.set_min_width(220.0);
                        if let Some(dir) = config::fonts_dir() {
                            // Clearly-a-button shortcut to the fonts folder so the
                            // user can drop .ttf files in without leaving the app.
                            if ui.button(tr!("open_folder")).clicked() {
                                let _ = std::fs::create_dir_all(&dir);
                                open_dir(&dir);
                                ui.close();
                            }
                            ui.separator();

                            // No empty-state label: an empty folder simply shows
                            // Open-folder above and Default below with nothing
                            // listed between them.
                            egui::ScrollArea::vertical()
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    for (stem, name) in &self.user_font_stems {
                                        let sel = self.cfg.view.font == *stem;
                                        let label = name.to_string();
                                        let resp = ui.selectable_label(sel, label);
                                        if resp.clicked() && !sel {
                                            self.cfg.view.font = stem.clone();
                                            install_ui_font(
                                                &ctx,
                                                &self.cfg.view.font,
                                                &self.user_font_stems,
                                            );
                                            ui.close();
                                        }
                                    }
                                });

                            ui.separator();
                            // Default (bottom): no user font selected — the table
                            // falls back to the built-in Ubuntu-Light face, the
                            // same as the menu.
                            let is_default = self.cfg.view.font.is_empty();
                            if ui.selectable_label(is_default, tr!("default")).clicked()
                                && !is_default
                            {
                                self.cfg.view.font.clear();
                                install_ui_font(&ctx, &self.cfg.view.font, &self.user_font_stems);
                                ui.close();
                            }
                        } else {
                            ui.label(tr!("config_unavailable"));
                        }
                    });

                    // ── Size submenu: preset point sizes ──────────────────
                    ui.menu_button(tr!("size"), |ui| {
                        let presets = [13.0, 14.0, 15.0, 16.0, 17.0, 18.0];
                        for &p in &presets {
                            let sel = (self.cfg.view.font_size - p).abs() < 0.01;
                            if ui.selectable_label(sel, format!("{:.0} pt", p)).clicked() {
                                self.cfg.view.font_size = p;
                                ui.close();
                            }
                        }
                    });
                });

                ui.menu_button(tr!("m_view"), |ui| {
                    ui.menu_button(tr!("columns"), |ui| {
                        // Keep at least one column visible: when only one remains,
                        // disable its checkbox so it can't be unchecked (hiding
                        // every column leaves an empty, dead-looking table).
                        let only_one = self.ui.visible_column_count() == 1;
                        ui.add_enabled(
                            !(only_one && self.ui.col_bookmark),
                            egui::Checkbox::new(&mut self.ui.col_bookmark, tr!("col_mark")),
                        )
                        .on_hover_text(tr!("col_mark_hover"));
                        ui.add_enabled(
                            !(only_one && self.ui.col_line),
                            egui::Checkbox::new(&mut self.ui.col_line, tr!("col_line")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_date),
                            egui::Checkbox::new(&mut self.ui.col_date, tr!("col_date")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_time),
                            egui::Checkbox::new(&mut self.ui.col_time, tr!("col_time")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_loglv),
                            egui::Checkbox::new(&mut self.ui.col_loglv, tr!("col_lv")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_pid),
                            egui::Checkbox::new(&mut self.ui.col_pid, tr!("col_pid")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_thread),
                            egui::Checkbox::new(&mut self.ui.col_thread, tr!("col_thread")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_uid),
                            egui::Checkbox::new(&mut self.ui.col_uid, tr!("col_uid")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_tag),
                            egui::Checkbox::new(&mut self.ui.col_tag, tr!("col_tag")),
                        );
                        ui.add_enabled(
                            !(only_one && self.ui.col_message),
                            egui::Checkbox::new(&mut self.ui.col_message, tr!("col_msg")),
                        );
                        ui.separator();
                        if ui.button(tr!("show_all")).clicked() {
                            self.ui.col_bookmark = true;
                            self.ui.col_line = true;
                            self.ui.col_date = true;
                            self.ui.col_time = true;
                            self.ui.col_loglv = true;
                            self.ui.col_pid = true;
                            self.ui.col_thread = true;
                            self.ui.col_uid = true;
                            self.ui.col_tag = true;
                            self.ui.col_message = true;
                            ui.close();
                        }
                    });
                    ui.menu_button(tr!("language"), |ui| {
                        let cur = self.cfg.view.lang.clone();
                        let opts: [(String, &str); 3] = [
                            (tr!("lang_auto"), "auto"),
                            (tr!("lang_en"), "en"),
                            (tr!("lang_zh"), "zh"),
                        ];
                        for (label, code) in &opts {
                            if ui.selectable_label(cur == *code, label.as_str()).clicked() {
                                self.set_lang(code);
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button(tr!("theme"), |ui| {
                        let cur = self.cfg.view.theme.clone();
                        let opts: [(&str, &str); 2] =
                            [("theme_light", "light"), ("theme_dark", "dark")];
                        for (label_key, value) in &opts {
                            if ui.selectable_label(cur == *value, tr!(label_key)).clicked() {
                                self.switch_theme(&ctx, value);
                                ui.close();
                            }
                        }
                    });
                });

                ui.menu_button(tr!("m_encoding"), |ui| {
                    for (label, value) in [(tr!("local"), "local"), ("UTF-8".to_string(), "utf-8")]
                    {
                        let selected = self.ui.encoding == value;
                        if ui.selectable_label(selected, label).clicked() {
                            if self.ui.encoding != value {
                                self.ui.encoding = value.into();
                                encoding_changed = true;
                            }
                            ui.close();
                        }
                    }
                });

                ui.menu_button(tr!("m_help"), |ui| {
                    if ui.link("LogFilter").clicked() {
                        ui.ctx().open_url(egui::OpenUrl {
                            url: "https://github.com/laomou/LogFilter".into(),
                            new_tab: true,
                        });
                        ui.close();
                    }
                });
            });
        });
        if let Some(p) = recent_open {
            if let Err(e) = self.open_file(&p) {
                self.status = tr!("status_failed_open", { e: &format!("{}", e) });
            }
        }
        // Re-decode the open file under the newly chosen encoding. open_file keeps
        // the column filters for the same path, so only the decoding changes.
        if encoding_changed {
            let current = self.model.read_recover().file_path.clone();
            if let Some(p) = current {
                if let Err(e) = self.open_file(&p) {
                    self.status = tr!("status_failed_open", { e: &format!("{}", e) });
                }
            }
        }
    }

    fn ui_options_panel(&mut self, ui: &mut egui::Ui) {
        // Option panel — 3 rows:
        //   Row 1: 🔍 Find (fills width)
        //   Row 2: Remove (half) · Highlight (half)
        //   Row 3: adb toolbar · Goto · Auto-scroll
        let ctx = ui.ctx().clone();
        let mut dirty = false;
        let mut goto_target: Option<usize> = None;
        egui::Panel::top("options").show(ui, |ui| {
            // Row 1: Find
            ui.horizontal(|ui| {
                dirty |= ui.checkbox(&mut self.ui.find_on, tr!("find")).changed();
                let w = (ui.available_width() - 8.0).max(200.0);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.ui.find)
                        .id(egui::Id::new("filter_find_edit"))
                        .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                        .desired_width(w),
                );
                dirty |= r.changed();
            });

            // Row 2: Remove | Highlight
            ui.horizontal(|ui| {
                let avail = ui.available_width();
                let text_w = (avail / 2.0 - 100.0).max(120.0);
                dirty |= ui.checkbox(&mut self.ui.remove_on, tr!("remove")).changed();
                dirty |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.ui.remove)
                            .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                            .desired_width(text_w),
                    )
                    .changed();
                ui.separator();
                // Highlight is purely visual — it never changes which rows are
                // shown, so it must NOT feed `dirty`/notify_filter (that clears the
                // row selection and forces a full refilter). refresh_highlight_caches()
                // picks up the new tokens every frame, so the edit shows up on its own.
                ui.checkbox(&mut self.ui.highlight_on, tr!("highlight"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.ui.highlight)
                        .id(egui::Id::new("filter_highlight_edit"))
                        .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                        .desired_width(text_w),
                );
                ui.separator();
                dirty |= ui
                    .checkbox(&mut self.ui.bookmarks_only, tr!("bookmarks_only"))
                    .on_hover_text(tr!("bookmarks_only_hover"))
                    .changed();
            });

            // Row 3: transport + adb/hdc toolbar + Goto + Auto-scroll
            ui.horizontal_wrapped(|ui| {
                let running = self.session.is_some();
                // Transport picker: adb (Android) vs hdc (HarmonyOS). Switching
                // changes the binary, the device-list command, and the -s/-t flag.
                let mut new_transport = self.transport;
                egui::ComboBox::from_id_salt("transport")
                    .selected_text(self.transport.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut new_transport,
                            Transport::Adb,
                            Transport::Adb.label(),
                        );
                        ui.selectable_value(
                            &mut new_transport,
                            Transport::Hdc,
                            Transport::Hdc.label(),
                        );
                    });
                if new_transport != self.transport {
                    self.transport = new_transport;
                    // Devices/keys differ per backend — drop the stale selection
                    // and re-probe with the new transport's list command.
                    self.selected_device.clear();
                    self.devices.clear();
                    // Also switch the command to one for the new backend (e.g.
                    // hilog for HarmonyOS) instead of leaving a logcat command
                    // selected that hdc can't run.
                    let new_cmds = self.cfg.device.commands(new_transport);
                    if !new_cmds.iter().any(|c| c == &self.selected_cmd) {
                        self.selected_cmd = new_cmds.first().cloned().unwrap_or_default();
                    }
                    self.refresh_devices(ctx.clone());
                }
                ui.separator();
                let cmds = self.cfg.device.commands(self.transport).to_vec();
                ui.label(tr!("cmd"));
                egui::ComboBox::from_id_salt("cmd")
                    .selected_text(&self.selected_cmd)
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for c in &cmds {
                            ui.selectable_value(&mut self.selected_cmd, c.clone(), c);
                        }
                    });
                ui.label(tr!("device"));
                let devices = self.devices.clone();
                egui::ComboBox::from_id_salt("device")
                    .selected_text(if self.selected_device.is_empty() {
                        tr!("device_any")
                    } else {
                        self.selected_device.clone()
                    })
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.selected_device,
                            String::new(),
                            tr!("device_any"),
                        );
                        for d in &devices {
                            ui.selectable_value(&mut self.selected_device, d.clone(), d);
                        }
                    });
                let refreshing_devices = self.device_refresh_rx.is_some();
                if ui
                    .add_enabled(!refreshing_devices, egui::Button::new("↻"))
                    .on_hover_text(tr!("refresh_devices"))
                    .clicked()
                {
                    self.refresh_devices(ctx.clone());
                }
                ui.separator();
                if ui
                    .button(if running { tr!("restart") } else { tr!("run") })
                    .clicked()
                {
                    self.run_session();
                }
                let pause_label = self
                    .session
                    .as_ref()
                    .map(|s| {
                        if s.is_paused() {
                            tr!("resume")
                        } else {
                            tr!("pause")
                        }
                    })
                    .unwrap_or(tr!("pause"));
                if ui
                    .add_enabled(running, egui::Button::new(pause_label))
                    .clicked()
                {
                    self.toggle_pause();
                }
                if ui
                    .add_enabled(running, egui::Button::new(tr!("stop")))
                    .clicked()
                {
                    self.stop_session();
                }
                if ui.button(tr!("clear")).clicked() {
                    self.clear();
                }
                ui.separator();
                ui.label(tr!("goto"));
                let goto_resp =
                    ui.add(egui::TextEdit::singleline(&mut self.ui.goto_line).desired_width(90.0));
                if goto_resp.has_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(n) = self.ui.goto_line.trim().parse::<usize>() {
                        if n > 0 {
                            goto_target = Some(n - 1);
                        }
                    }
                }
                // Toggling Auto-scroll ON jumps to the bottom right away, so the
                // switch takes effect immediately even when scrolled up (egui's
                // stick-to-bottom otherwise only re-engages once you're already at
                // the end). The table consumes pending_scroll later this same frame.
                if ui
                    .checkbox(&mut self.auto_scroll, tr!("auto_scroll"))
                    .changed()
                    && self.auto_scroll
                {
                    let len = self.model.read_recover().filtered.len();
                    if len > 0 {
                        self.pending_scroll = Some(len - 1);
                    }
                }
            });
        });
        if dirty {
            self.notify_filter();
        }
        if let Some(row) = goto_target {
            self.goto_line(row);
        }
    }

    /// Jump to the entry at zero-based index `row` (the user typed `row + 1`).
    /// Three outcomes, each with explicit feedback instead of silently no-oping:
    ///   * out of range          → status message, no scroll
    ///   * visible in `filtered` → select + scroll to it
    ///   * hidden by the filter  → status message + scroll to nearest visible row
    fn goto_line(&mut self, row: usize) {
        let m = self.model.read_recover();
        let total = m.entries.len();
        let line_no = row + 1;
        if row >= total {
            drop(m);
            self.status = tr!("status_goto_out_of_range", {
                n: &line_no.to_string(),
                total: &total.to_string()
            });
        } else if let Some(pos) = m.filtered.iter().position(|&e| e as usize == row) {
            drop(m);
            self.pending_scroll = Some(pos);
            self.selected_rows.clear();
            self.selected_rows.insert(pos);
            // Anchor/cursor must follow the jump too, or subsequent Arrow /
            // Shift+Arrow navigation moves from the stale previous position
            // instead of the row the user just jumped to.
            self.selection_anchor = Some(pos);
            self.selection_cursor = Some(pos);
        } else {
            // The line exists but is hidden by the active filter. Rather than
            // silently doing nothing, tell the user and scroll to the nearest
            // visible row so they still get spatial context. `filtered` is
            // strictly ascending (built by scanning entries in order), so a
            // partition point locates the neighbour without a linear scan.
            if !m.filtered.is_empty() {
                let nearest = m
                    .filtered
                    .partition_point(|&e| (e as usize) < row)
                    .min(m.filtered.len() - 1);
                self.pending_scroll = Some(nearest);
            }
            drop(m);
            self.status = tr!("status_goto_filtered", { n: &line_no.to_string() });
        }
    }

    /// Clear a transient status message once it has been shown for a few seconds,
    /// so old messages ("Copied…", "Saved…") don't linger after Clear/open/adb and
    /// don't permanently hide the "Selected N" readout. `now` is the egui frame
    /// time (seconds). Resets the timer whenever the message changes.
    fn tick_status(&mut self, now: f64) {
        const STATUS_TTL_SECS: f64 = 5.0;
        if self.status != self.last_status_seen {
            self.last_status_seen = self.status.clone();
            self.status_shown_at = now;
        }
        if !self.status.is_empty() && now - self.status_shown_at > STATUS_TTL_SECS {
            self.status.clear();
            self.last_status_seen.clear();
        }
    }

    fn ui_status_bar(&mut self, ui: &mut egui::Ui) {
        // Expire stale status messages so "Selected N" can reappear. Request a
        // repaint while one is showing so it clears on time even when idle.
        let now = ui.input(|i| i.time);
        self.tick_status(now);
        if !self.status.is_empty() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }
        // Status bar
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            let model = self.model.read_recover();
            ui.horizontal(|ui| {
                match &model.file_path {
                    Some(p) => {
                        // Only the file name, middle-ellipsized to fit the bar;
                        // full path on hover. Budget is in logical points, so it
                        // scales with window size and DPI.
                        let name = p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| p.display().to_string());
                        let budget = (ui.available_width() * 0.5).max(80.0);
                        let full = p.display().to_string();
                        ui.label(fit_middle(ui, &name, budget)).on_hover_ui(|ui| {
                            // Single-line tooltip: don't wrap the full path.
                            ui.add(egui::Label::new(full).wrap_mode(egui::TextWrapMode::Extend));
                        });
                    }
                    None => {
                        ui.label(tr!("no_file"));
                    }
                }
                ui.separator();
                ui.label(format!("{} {}", tr!("total"), model.entries.len()));
                ui.separator();
                // When entries are loaded but the active filters hide every one of
                // them, color the filtered count so the empty table has a visible
                // cause (a typo'd Find, a stale column filter, or "bookmarks only"
                // with no bookmarks all otherwise look identical).
                let filtered_text = format!("{} {}", tr!("filtered"), model.filtered.len());
                if !model.entries.is_empty() && model.filtered.is_empty() {
                    ui.label(
                        egui::RichText::new(filtered_text)
                            .color(Color32::from_rgb(230, 126, 34))
                            .strong(),
                    );
                } else {
                    ui.label(filtered_text);
                }
                ui.separator();
                ui.label(format!("{} {}", tr!("bookmarks"), model.bookmarks.len()));
                // Encoding only applies to a loaded file (resolves "Local" to the
                // sniffed codepage). An adb stream is always UTF-8-lossy and an
                // empty view has no source, so showing an encoding there is stale
                // or misleading — omit it unless a file is loaded.
                if model.file_path.is_some() {
                    ui.separator();
                    let detected = self.detected_encoding.lock_recover().clone();
                    let enc_label = detected.unwrap_or_else(|| self.ui.encoding.clone());
                    ui.label(enc_label.to_uppercase());
                }
                let n = self.selected_rows.len();
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(&self.status);
                } else if n > 0 {
                    ui.separator();
                    ui.label(tr!("selected_n", { n: &n.to_string() }));
                }
            });
        });
    }

    fn ui_indicator(&mut self, ui: &mut egui::Ui) {
        // Indicator panel (mini-scrollbar)
        egui::Panel::right("indicator")
            .exact_size(24.0)
            .resizable(false)
            .show(ui, |ui| {
                let (rect, response) =
                    ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, Color32::from_gray(30));
                let model = self.model.read_recover();
                let total = model.filtered.len();
                if total > 0 {
                    let h = rect.height();
                    let left_col = egui::Rect::from_min_max(
                        rect.min,
                        egui::pos2(rect.min.x + rect.width() * 0.5, rect.max.y),
                    );
                    let right_col = egui::Rect::from_min_max(
                        egui::pos2(rect.min.x + rect.width() * 0.5, rect.min.y),
                        rect.max,
                    );
                    let paint_mark = |fi: usize, col: egui::Rect, color: Color32| {
                        let y = col.min.y + h * (fi as f32) / (total as f32);
                        painter.rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(col.min.x, y),
                                egui::vec2(col.width(), 2.0),
                            ),
                            0.0,
                            color,
                        );
                    };
                    for &ei in &model.bookmarks {
                        // `filtered` is built by scanning entries in ascending order.
                        if let Ok(fi) = model.filtered.binary_search(&ei) {
                            paint_mark(fi, left_col, Color32::from_rgb(80, 140, 255));
                        }
                    }
                    for &ei in &model.error_lines {
                        // `filtered` is built by scanning entries in ascending order.
                        if let Ok(fi) = model.filtered.binary_search(&ei) {
                            paint_mark(fi, right_col, Color32::from_rgb(255, 80, 80));
                        }
                    }
                    // Handle click to jump
                    if let Some(pos) = response.interact_pointer_pos() {
                        let frac = ((pos.y - rect.min.y) / h).clamp(0.0, 1.0);
                        let target = (frac * total as f32) as usize;
                        self.pending_scroll = Some(target.min(total.saturating_sub(1)));
                        self.selected_rows.clear();
                        self.selected_rows.insert(self.pending_scroll.unwrap_or(0));
                        self.selection_anchor = self.pending_scroll;
                        self.selection_cursor = self.pending_scroll;
                    }
                }
            });
    }

    fn ui_table(&mut self, ui: &mut egui::Ui) {
        // Log table
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            let font = FontId::monospace(self.cfg.view.font_size);
            self.refresh_highlight_caches();
            let highlight_palette: &[Color32] = &self.cached_highlight_palette;
            let highlight_tokens: &[String] = &self.cached_highlight_tokens;
            let find_tokens: &[String] = &self.cached_find_tokens;

            let (cl, cd, ct, clv, cpi, cth, cui, cta, cmk, cms) = (
                tr!("col_line"),
                tr!("col_date"),
                tr!("col_time"),
                tr!("col_lv"),
                tr!("col_pid"),
                tr!("col_thread"),
                tr!("col_uid"),
                tr!("col_tag"),
                tr!("col_mark"),
                tr!("col_message"),
            );
            let cols_show: [(bool, &str, f32); 10] = [
                (self.ui.col_line, &cl, self.cached_col_widths[0]),
                (self.ui.col_date, &cd, self.cached_col_widths[1]),
                (self.ui.col_time, &ct, self.cached_col_widths[2]),
                (self.ui.col_loglv, &clv, self.cached_col_widths[3]),
                (self.ui.col_pid, &cpi, self.cached_col_widths[4]),
                (self.ui.col_thread, &cth, self.cached_col_widths[5]),
                (self.ui.col_uid, &cui, self.cached_col_widths[6]),
                (self.ui.col_tag, &cta, self.cached_col_widths[7]),
                (self.ui.col_bookmark, &cmk, self.cached_col_widths[8]),
                (self.ui.col_message, &cms, self.cached_col_widths[9]),
            ];
            let last_visible = cols_show.iter().rposition(|(v, _, _)| *v);
            let table_available_height = ui.available_height();
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                // Follow new rows as they stream in (adb / file tailing) while
                // "Auto-scroll" is on. egui only pins to the bottom while the view
                // is already there, releases the moment the user scrolls up, and
                // defers to an explicit scroll_to_row (goto/arrows) — so it never
                // fights manual navigation.
                .stick_to_bottom(self.auto_scroll)
                // Fill all available vertical space instead of egui_extras'
                // default 800px cap / content-shrink, so the table uses the whole
                // available window.
                .auto_shrink([false, false])
                .max_scroll_height(f32::INFINITY)
                // Always show the vertical scrollbar: keeps a stable gutter (no
                // remainder-column reflow when it would otherwise toggle) and is
                // the preferred look here.
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .sense(egui::Sense::click());
            for (i, (visible, _, w)) in cols_show.iter().enumerate() {
                if !*visible {
                    continue;
                }
                if Some(i) == last_visible {
                    table = table.column(Column::remainder().at_least(*w));
                } else {
                    table = table.column(Column::initial(*w).at_least(*w * 0.5));
                }
            }
            if let Some(scroll_to) = self.pending_scroll.take() {
                table = table.scroll_to_row(scroll_to, Some(egui::Align::Center));
            }
            let sel = self.selected_rows.clone();

            let model = self.model.read_recover();
            let show_empty_shortcuts = model.entries.is_empty();
            let entries = &model.entries;
            let filtered = &model.filtered;
            let bookmarks = &model.bookmarks;
            let use_highlight = !highlight_tokens.is_empty() || !find_tokens.is_empty();

            let mut clicked_row: Option<usize> = None;
            let mut double_clicked_row: Option<usize> = None;
            let mut alt_left_tag: Option<String> = None;
            let mut alt_right_tag: Option<String> = None;
            let mut copy_cell_text: Option<String> = None;
            let alt = ctx.input(|i| i.modifiers.alt);
            let mut open_picker: Option<(PickerCol, egui::Pos2)> = None;
            let mut hide_col_idx: Option<usize> = None;
            // Widths captured from body.widths() this frame; mapped back to the
            // 10-slot cached_col_widths array below (visible columns only).
            let mut new_col_widths: Option<Vec<f32>> = None;

            // Column meta for header interactions
            #[derive(Clone, Copy)]
            enum ColKind {
                Line,
                Date,
                Time,
                Lv,
                Pid,
                Thread,
                Uid,
                Tag,
                Bookmark,
                Message,
            }
            let col_kinds: [ColKind; 10] = [
                ColKind::Line,
                ColKind::Date,
                ColKind::Time,
                ColKind::Lv,
                ColKind::Pid,
                ColKind::Thread,
                ColKind::Uid,
                ColKind::Tag,
                ColKind::Bookmark,
                ColKind::Message,
            ];
            let picker_of = |k: ColKind| -> Option<PickerCol> {
                match k {
                    ColKind::Lv => Some(PickerCol::Level),
                    ColKind::Pid => Some(PickerCol::Pid),
                    ColKind::Thread => Some(PickerCol::Tid),
                    ColKind::Tag => Some(PickerCol::Tag),
                    _ => None,
                }
            };

            // Row/header heights scale with the font so larger sizes don't clip.
            // ~1.35× line-height matches egui's default proportions for tables.
            let font_size = self.cfg.view.font_size;
            let row_h = (font_size * 1.35).ceil().max(16.0);
            let header_h = (font_size * 1.6).ceil().max(20.0);
            let available_rows = ((table_available_height - header_h) / row_h).floor() as usize;
            self.visible_table_rows = available_rows.max(1);

            table
                .header(header_h, |mut h| {
                    for (i, (visible, name, _)) in cols_show.iter().enumerate() {
                        if !*visible {
                            continue;
                        }
                        let kind = col_kinds[i];
                        let pk = picker_of(kind);
                        // Dropdown marker: use ▼ (U+25BC) — a universally recognized
                        // "click for menu" symbol rendered from the Proportional
                        // font fallback chain (Monospace now mirrors Proportional).
                        let label = if pk.is_some() {
                            format!("{name} ▼")
                        } else {
                            name.to_string()
                        };
                        h.col(|ui| {
                            let resp = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&label).font(font.clone()).strong(),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if let Some(p) = pk {
                                if resp.clicked() {
                                    open_picker = Some((p, resp.rect.left_bottom()));
                                }
                            }
                            resp.context_menu(|ui| {
                                let can_filter = pk.is_some();
                                if ui
                                    .add_enabled(can_filter, egui::Button::new(tr!("filter_this")))
                                    .clicked()
                                {
                                    if let Some(p) = pk {
                                        open_picker = Some((p, resp.rect.left_bottom()));
                                    }
                                    ui.close();
                                }
                                ui.separator();
                                // Forbid hiding the last visible column (keeps the
                                // table and its headers usable).
                                let only_one_col =
                                    cols_show.iter().filter(|(v, _, _)| *v).count() == 1;
                                if ui
                                    .add_enabled(!only_one_col, egui::Button::new(tr!("hide_this")))
                                    .clicked()
                                {
                                    hide_col_idx = Some(i);
                                    ui.close();
                                }
                            });
                        });
                    }
                })
                .body(|body| {
                    // Capture current column widths (visible columns only) so we
                    // can persist them on exit via cached_col_widths.
                    new_col_widths = Some(body.widths().to_vec());
                    if show_empty_shortcuts {
                        let shortcut_rows = &self.cached_shortcut_rows;
                        let row_count = EMPTY_SHORTCUT_TOP_PADDING_ROWS + shortcut_rows.len();
                        body.rows(row_h, row_count, |mut row| {
                            if row.index() < EMPTY_SHORTCUT_TOP_PADDING_ROWS {
                                for (visible, _, _) in cols_show.iter() {
                                    if *visible {
                                        row.col(|_| {});
                                    }
                                }
                                return;
                            }
                            let shortcut =
                                &shortcut_rows[row.index() - EMPTY_SHORTCUT_TOP_PADDING_ROWS];
                            let text = |s: &str| {
                                egui::RichText::new(s)
                                    .font(font.clone())
                                    .color(Color32::DARK_GRAY)
                            };
                            if self.ui.col_line {
                                row.col(|_| {});
                            }
                            if self.ui.col_date {
                                row.col(|_| {});
                            }
                            if self.ui.col_time {
                                row.col(|_| {});
                            }
                            if self.ui.col_loglv {
                                row.col(|ui| {
                                    ui.add(egui::Label::new(text("I")).truncate());
                                });
                            }
                            if self.ui.col_pid {
                                row.col(|_| {});
                            }
                            if self.ui.col_thread {
                                row.col(|_| {});
                            }
                            if self.ui.col_uid {
                                row.col(|_| {});
                            }
                            if self.ui.col_tag {
                                row.col(|ui| {
                                    ui.add(egui::Label::new(text(&shortcut.tag)).truncate());
                                });
                            }
                            if self.ui.col_bookmark {
                                row.col(|_| {});
                            }
                            if self.ui.col_message {
                                row.col(|ui| {
                                    ui.add(egui::Label::new(text(&shortcut.message)).truncate());
                                });
                            }
                        });
                        return;
                    }
                    body.rows(row_h, filtered.len(), |mut row| {
                        let row_idx = row.index();
                        let entry_idx = filtered[row_idx];
                        let e = &entries[entry_idx as usize];
                        let col = level_color(e.level, &self.cached_level_colors);
                        let is_selected = sel.contains(&row_idx);
                        let is_bookmarked = bookmarks.contains(&entry_idx);
                        row.set_selected(is_selected);

                        // Render each cell with the configured monospace font so
                        // View → Font size affects *every* column, not just Tag/Message.
                        // truncate() keeps every cell on a single line (… on overflow).
                        let render = |ui: &mut egui::Ui, s: &str| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(s).font(font.clone()).color(col),
                                )
                                .truncate(),
                            );
                        };

                        if self.ui.col_line {
                            row.col(|ui| {
                                render(ui, &e.line_no.to_string());
                            });
                        }
                        if self.ui.col_date {
                            row.col(|ui| {
                                render(ui, e.date());
                            });
                        }
                        if self.ui.col_time {
                            row.col(|ui| {
                                render(ui, e.time());
                            });
                        }
                        if self.ui.col_loglv {
                            row.col(|ui| {
                                render(ui, &e.level.as_char().to_string());
                            });
                        }
                        if self.ui.col_pid {
                            row.col(|ui| {
                                render(ui, e.pid());
                            });
                        }
                        if self.ui.col_thread {
                            row.col(|ui| {
                                render(ui, e.tid());
                            });
                        }
                        if self.ui.col_uid {
                            row.col(|ui| {
                                render(ui, e.uid());
                            });
                        }
                        if self.ui.col_tag {
                            // Render a plain (non-interactive) label so the *cell*
                            // keeps the pointer hover — an inner Sense::click label
                            // would steal it and kill the row-hover highlight.
                            let (_, resp) = row.col(|ui| {
                                if use_highlight {
                                    let job = build_highlighted(
                                        e.tag(),
                                        highlight_tokens,
                                        find_tokens,
                                        col,
                                        font.clone(),
                                        highlight_palette,
                                    );
                                    ui.add(egui::Label::new(job).truncate());
                                } else {
                                    render(ui, e.tag());
                                }
                            });
                            if alt && resp.clicked() {
                                alt_left_tag = Some(e.tag().to_string());
                            } else if resp.clicked() {
                                // Plain click on the Tag cell selects the row.
                                clicked_row = Some(row_idx);
                            }
                            if resp.double_clicked() {
                                double_clicked_row = Some(row_idx);
                            }
                            if alt && resp.secondary_clicked() {
                                alt_right_tag = Some(e.tag().to_string());
                            }
                        }
                        if self.ui.col_bookmark {
                            row.col(|ui| {
                                if is_bookmarked {
                                    ui.add(egui::Label::new(
                                        egui::RichText::new("★")
                                            .font(font.clone())
                                            .color(Color32::from_rgb(80, 140, 255)),
                                    ));
                                }
                            });
                        }
                        if self.ui.col_message {
                            let (_, resp) = row.col(|ui| {
                                if use_highlight {
                                    let job = build_highlighted(
                                        e.message(),
                                        highlight_tokens,
                                        find_tokens,
                                        col,
                                        font.clone(),
                                        highlight_palette,
                                    );
                                    ui.add(egui::Label::new(job).truncate());
                                } else {
                                    render(ui, e.message());
                                }
                            });
                            // The cell senses clicks (table Sense::click); a plain
                            // left-click selects the row.
                            if resp.clicked() {
                                clicked_row = Some(row_idx);
                            }
                            if resp.double_clicked() {
                                double_clicked_row = Some(row_idx);
                            }
                            resp.context_menu(|ui| {
                                if ui.button(tr!("copy_message")).clicked() {
                                    if self.selected_rows.len() > 1 {
                                        copy_cell_text = Some(Self::copy_selected_column_text(
                                            entries,
                                            filtered,
                                            &self.selected_rows,
                                            |e| e.message(),
                                        ));
                                    } else {
                                        copy_cell_text = Some(e.message().to_string());
                                    }
                                    ui.close();
                                }
                                if ui.button(tr!("copy_row")).clicked() {
                                    if self.selected_rows.len() > 1 {
                                        copy_cell_text = Some(self.copy_selected_rows_text());
                                    } else {
                                        copy_cell_text = Some(self.visible_row_text(e));
                                    }
                                    ui.close();
                                }
                            });
                        }

                        let response = row.response();
                        if response.clicked() {
                            clicked_row = Some(row_idx);
                        }
                        if response.double_clicked() {
                            double_clicked_row = Some(row_idx);
                        }
                    });
                });

            let ctrl_or_cmd = ctx.input(|i| i.modifiers.command || i.modifiers.ctrl);
            let shift = ctx.input(|i| i.modifiers.shift);

            // Map visible-column widths back to the 10-slot array.
            if let Some(widths) = new_col_widths {
                let mut wi = 0usize;
                for (i, (visible, _, _)) in cols_show.iter().enumerate() {
                    if *visible {
                        if let Some(&w) = widths.get(wi) {
                            self.cached_col_widths[i] = w;
                        }
                        wi += 1;
                    }
                }
            }

            drop(model);
            // Multi-select: Ctrl/Cmd+click toggles; Shift+click selects range;
            // plain click replaces selection with single row.
            if let Some(r) = clicked_row {
                if ctrl_or_cmd {
                    if self.selected_rows.contains(&r) {
                        self.selected_rows.remove(&r);
                    } else {
                        self.selected_rows.insert(r);
                    }
                    self.status =
                        tr!("status_ctrl_click", { n: &self.selected_rows.len().to_string() });
                } else if shift {
                    // Use the stored anchor (fixed end), not an arbitrary HashSet
                    // element. Clamp it to the current filtered length so a stale
                    // anchor can never expand the range past the visible rows
                    // (mirrors the keyboard extend_selection clamp).
                    let len = self.model.read_recover().filtered.len();
                    let max = len.saturating_sub(1);
                    let anchor = self.selection_anchor.unwrap_or(r).min(max);
                    let r = r.min(max);
                    let (lo, hi) = if r < anchor { (r, anchor) } else { (anchor, r) };
                    for i in lo..=hi {
                        self.selected_rows.insert(i);
                    }
                    self.selection_cursor = Some(r);
                    self.status =
                        tr!("status_shift_click", { n: &self.selected_rows.len().to_string() });
                } else {
                    self.selected_rows.clear();
                    self.selected_rows.insert(r);
                    self.selection_anchor = Some(r);
                    self.selection_cursor = Some(r);
                }
            }
            if let Some(r) = double_clicked_row {
                let entry_idx = self.model.read_recover().filtered.get(r).copied();
                if let Some(i) = entry_idx {
                    self.toggle_bookmark(i);
                }
                self.selected_rows.clear();
                self.selected_rows.insert(r);
                self.selection_anchor = Some(r);
                self.selection_cursor = Some(r);
            }
            if let Some(t) = alt_left_tag {
                self.add_show_tag(&t);
            }
            if let Some(t) = alt_right_tag {
                self.add_remove_tag(&t);
            }
            if let Some(txt) = copy_cell_text {
                let n = txt.lines().count();
                self.copy_text_to_clipboard(&txt, n);
            }

            // Hide column requested from column-header context menu.
            if let Some(idx) = hide_col_idx {
                match idx {
                    0 => self.ui.col_line = false,
                    1 => self.ui.col_date = false,
                    2 => self.ui.col_time = false,
                    3 => self.ui.col_loglv = false,
                    4 => self.ui.col_pid = false,
                    5 => self.ui.col_thread = false,
                    6 => self.ui.col_uid = false,
                    7 => self.ui.col_tag = false,
                    8 => self.ui.col_bookmark = false,
                    9 => self.ui.col_message = false,
                    _ => {}
                }
            }
            // Open picker requested from column-header click or context menu.
            if let Some((col, anchor)) = open_picker {
                self.ui.picker = Some(PickerState {
                    col,
                    search: String::new(),
                    anchor,
                    just_opened: true,
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn send_utf8_lines_replaces_invalid_bytes_and_continues() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "logfilter-invalid-utf8-{}-{unique}.log",
            std::process::id()
        ));
        std::fs::write(
            &path,
            [
                &[0xEF, 0xBB, 0xBF, b'f', b'i', b'r', b's', b't', b'\r', b'\n'][..],
                &[b'b', b'a', b'd', b' ', 0xFF, b'\r', b'\n'][..],
                b"last",
            ]
            .concat(),
        )
        .unwrap();

        let (tx, rx) = bounded(8);
        let source_epoch = Arc::new(AtomicU64::new(42));
        let file = std::fs::File::open(&path).unwrap();
        send_utf8_lines(file, tx, 42, source_epoch).expect("clean read should return Ok");
        let lines: Vec<String> = rx.try_iter().map(|(_, line)| line).collect();

        let _ = std::fs::remove_file(path);
        assert_eq!(lines, vec!["first", "bad \u{fffd}", "last"]);
    }

    #[test]
    fn adjusted_table_font_size_clamps_to_bounds() {
        assert_eq!(adjusted_table_font_size(13.0, 1.0), 14.0);
        assert_eq!(adjusted_table_font_size(12.0, 0.0), 13.0);
        assert_eq!(adjusted_table_font_size(17.5, 2.0), 18.0);
    }

    #[test]
    fn reset_table_font_size_uses_config_default() {
        assert_eq!(Config::default().view.font_size, 13.0);
    }

    #[test]
    fn ui_state_round_trips_columns_and_filter_toggles() {
        let mut cfg = Config::default();
        let mut ui = UiState::from_config(&cfg);
        // Tweak column visibility and filter toggles away from defaults.
        ui.col_uid = true; // was hidden
        ui.col_date = false; // was shown
        ui.col_bookmark = true; // was hidden
        ui.find = "needle".into();
        ui.find_on = false; // text present but explicitly disabled
        ui.highlight_on = true;

        ui.write_back(&mut cfg);
        let restored = UiState::from_config(&cfg);

        assert!(restored.col_uid, "UID visibility should persist");
        assert!(!restored.col_date, "Date hidden should persist");
        assert!(restored.col_bookmark, "Bookmark column should persist");
        assert_eq!(restored.find, "needle");
        assert!(
            !restored.find_on,
            "a disabled Find must persist even with text present"
        );
        assert!(restored.highlight_on);
    }

    #[test]
    fn old_config_infers_filter_on_from_text() {
        // find_on == None (old config) → derive from whether the text is non-empty.
        let mut cfg = Config::default();
        cfg.filters.find = "hello".into();
        cfg.filters.find_on = None;
        cfg.filters.remove = String::new();
        cfg.filters.remove_on = None;
        let ui = UiState::from_config(&cfg);
        assert!(ui.find_on, "non-empty Find text → on");
        assert!(!ui.remove_on, "empty Remove text → off");
    }

    #[test]
    fn visible_column_count_tracks_visible_columns() {
        let mut ui = UiState::from_config(&Config::default());
        // Hide everything, then reveal one at a time.
        ui.col_bookmark = false;
        ui.col_line = false;
        ui.col_date = false;
        ui.col_time = false;
        ui.col_loglv = false;
        ui.col_pid = false;
        ui.col_thread = false;
        ui.col_uid = false;
        ui.col_tag = false;
        ui.col_message = false;
        assert_eq!(ui.visible_column_count(), 0);

        ui.col_message = true;
        assert_eq!(ui.visible_column_count(), 1);
        // With one column left, the guard disables its checkbox / "Hide this":
        // `only_one && col_message` is true, so add_enabled(false) is passed.
        assert!(ui.visible_column_count() == 1 && ui.col_message);

        ui.col_tag = true;
        assert_eq!(ui.visible_column_count(), 2);
    }

    #[test]
    fn highlight_palette_cache_refreshes_when_same_length_colors_change() {
        let ctx = egui::Context::default();
        let mut app = App::new_for_test(&ctx);

        app.cfg.colors.highlights = vec!["0xFF0000".into()];
        app.refresh_highlight_caches();
        assert_eq!(
            app.cached_highlight_palette,
            vec![Color32::from_rgb(255, 0, 0)]
        );

        // Regression: changing a color without changing the number of palette
        // entries must invalidate the parsed-color cache.
        app.cfg.colors.highlights = vec!["0x0000FF".into()];
        app.refresh_highlight_caches();
        assert_eq!(
            app.cached_highlight_palette,
            vec![Color32::from_rgb(0, 0, 255)]
        );
        assert_eq!(app.cached_highlight_palette_raw, vec!["0x0000FF"]);
    }

    #[test]
    fn clamp_filtered_row_handles_empty_and_bounds() {
        assert_eq!(clamp_filtered_row(0, 0), None);
        assert_eq!(clamp_filtered_row(0, 3), Some(0));
        assert_eq!(clamp_filtered_row(2, 3), Some(2));
        assert_eq!(clamp_filtered_row(usize::MAX, 3), Some(2));
    }

    #[test]
    fn page_row_moves_by_visible_rows_and_clamps() {
        assert_eq!(page_row(None, 0, 10, true), None);
        assert_eq!(page_row(None, 100, 10, true), Some(10));
        assert_eq!(page_row(Some(50), 100, 10, false), Some(40));
        assert_eq!(page_row(Some(95), 100, 10, true), Some(99));
        assert_eq!(page_row(Some(5), 100, 10, false), Some(0));
        assert_eq!(page_row(Some(5), 100, 0, true), Some(6));
    }

    #[test]
    fn detect_format_from_cmd_variants() {
        assert_eq!(
            detect_format_from_cmd("logcat -v threadtime"),
            LogFormat::ThreadTime
        );
        assert_eq!(
            detect_format_from_cmd("logcat -v long"),
            LogFormat::ThreadTime
        );
        assert_eq!(detect_format_from_cmd("logcat -v time"), LogFormat::Time);
        assert_eq!(detect_format_from_cmd("logcat -v brief"), LogFormat::Brief);
        assert_eq!(
            detect_format_from_cmd("logcat -v process"),
            LogFormat::Brief
        );
        assert_eq!(detect_format_from_cmd("logcat -v tag"), LogFormat::Brief);
        assert_eq!(
            detect_format_from_cmd("shell cat /proc/kmsg"),
            LogFormat::Kernel
        );
        assert_eq!(detect_format_from_cmd("logcat"), LogFormat::Unknown);
        assert_eq!(
            detect_format_from_cmd("logcat -v nonsense"),
            LogFormat::Unknown
        );
        // Real (built-in) hilog commands resolve to HiLog via the command table.
        assert_eq!(
            detect_format_from_cmd("hilog -v threadtime -r"),
            LogFormat::HiLog
        );
        assert_eq!(
            detect_format_from_cmd("hilog -D 0x2F00 -v time"),
            LogFormat::HiLog
        );
        // A bare, non-shipped string isn't in the table and has no -v flag → Unknown.
        assert_eq!(detect_format_from_cmd("hilog"), LogFormat::Unknown);
    }

    #[test]
    fn device_refresh_result_preserves_existing_selection_when_present() {
        let ctx = egui::Context::default();
        let mut app = App::new_for_test(&ctx);
        app.selected_device = "keep".into();

        app.apply_devices_result(Ok(vec!["other".into(), "keep".into()]));

        assert_eq!(app.devices, vec!["other", "keep"]);
        assert_eq!(app.selected_device, "keep");
    }

    #[test]
    fn poll_device_refresh_applies_completed_result() {
        let ctx = egui::Context::default();
        let mut app = App::new_for_test(&ctx);
        let (tx, rx) = bounded(1);
        tx.send(Ok(vec!["serial-1".into()])).unwrap();
        app.device_refresh_rx = Some(rx);

        app.poll_device_refresh();

        assert_eq!(app.devices, vec!["serial-1"]);
        assert!(app.device_refresh_rx.is_none());
    }
}

/// Integration tests using egui_kittest to drive real user interactions
/// (key presses, button clicks) through the full egui event → App::ui →
/// state change pipeline. These test what actually broke before: shortcuts,
/// selection, bookmarks, toolbar buttons.
#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::model::LogEntry;
    use egui_kittest::kittest::Queryable as _;
    use egui_kittest::Harness;

    fn harness<'a>() -> Harness<'a, App> {
        Harness::builder()
            .with_size(egui::vec2(1350.0, 720.0))
            .build_eframe(|cc| App::new_for_test(&cc.egui_ctx))
    }

    fn inject(app: &mut App, n: usize) {
        use crate::model::LevelMask;
        let mut m = app.model.write().unwrap();
        for i in 0..n {
            let lv = if i % 5 == 0 {
                LevelMask::E
            } else {
                LevelMask::I
            };
            let tag = format!("Tag{}", i % 3);
            m.append(LogEntry::from_fields(
                "07-20",
                "12:00:00.000",
                lv,
                "100",
                "200",
                &tag,
                &format!("msg {i}"),
            ));
        }
        m.filtered = (0..n as u32).collect();
    }

    // ─── Keyboard event simulation (full pipeline) ───────────────────────

    #[test]
    fn arrow_down_key_moves_selection() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        // Give the app a starting selection so ArrowDown has somewhere to go
        h.state_mut().select_filtered_row_with_len(0, 20);
        h.run();

        // Simulate pressing the Down arrow key — goes through egui's event
        // system → App::handle_shortcuts → consume_key → move_selected_row
        h.key_press(egui::Key::ArrowDown);
        h.run();

        assert!(
            h.state().selected_rows.contains(&1),
            "ArrowDown from row 0 should select row 1, got {:?}",
            h.state().selected_rows
        );
        assert_eq!(h.state().selection_cursor, Some(1));
    }

    #[test]
    fn first_arrow_down_selects_row_zero() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.run();
        // No selection yet (fresh load).
        assert!(h.state().selected_rows.is_empty());
        assert_eq!(h.state().selection_cursor, None);

        h.key_press(egui::Key::ArrowDown);
        h.run();

        assert!(
            h.state().selected_rows.contains(&0),
            "first ArrowDown with no selection should land on row 0, got {:?}",
            h.state().selected_rows
        );
        assert_eq!(h.state().selection_cursor, Some(0));
    }

    #[test]
    fn arrow_up_key_clamps_at_zero() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 10);
        h.state_mut().select_filtered_row_with_len(0, 10);
        h.run();

        h.key_press(egui::Key::ArrowUp);
        h.run();

        assert!(h.state().selected_rows.contains(&0), "should stay at 0");
    }

    #[test]
    fn shift_arrow_down_extends_selection() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.state_mut().select_filtered_row_with_len(5, 20);
        h.run();

        // Shift+Down × 3 — through real egui event pipeline
        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowDown);
        h.run();
        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowDown);
        h.run();
        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowDown);
        h.run();

        // Should have 4 rows selected: 5,6,7,8
        let sel = &h.state().selected_rows;
        assert_eq!(sel.len(), 4, "expected 4 selected rows, got {}", sel.len());
        for r in 5..=8 {
            assert!(sel.contains(&r), "row {r} should be selected");
        }
        assert_eq!(h.state().selection_anchor, Some(5));
        assert_eq!(h.state().selection_cursor, Some(8));
    }

    #[test]
    fn shift_arrow_up_extends_backward() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.state_mut().select_filtered_row_with_len(10, 20);
        h.run();

        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowUp);
        h.run();
        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowUp);
        h.run();

        let sel = &h.state().selected_rows;
        assert_eq!(sel.len(), 3); // 8,9,10
        for r in 8..=10 {
            assert!(sel.contains(&r));
        }
    }

    #[test]
    fn ctrl_f2_toggles_bookmark_via_keyboard() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.state_mut().select_filtered_row_with_len(7, 20);
        h.run();

        // Ctrl+F2 → toggle_selected_bookmark
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F2);
        h.run();

        // Entry 7 should now be bookmarked
        assert!(
            h.state().model.read().unwrap().bookmarks.contains(&7),
            "Ctrl+F2 should bookmark entry at selected row"
        );

        // Press again → un-bookmark
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F2);
        h.run();
        assert!(!h.state().model.read().unwrap().bookmarks.contains(&7));
    }

    #[test]
    fn f3_jumps_to_next_bookmark() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 30);
        h.state_mut().toggle_bookmark(10);
        h.state_mut().toggle_bookmark(20);
        h.state_mut().select_filtered_row_with_len(0, 30);
        h.run();

        // F3 = next bookmark
        h.key_press(egui::Key::F3);
        h.run();
        assert!(
            h.state().selected_rows.contains(&10),
            "F3 should jump to bookmark at 10"
        );

        h.key_press(egui::Key::F3);
        h.run();
        assert!(
            h.state().selected_rows.contains(&20),
            "F3 again should jump to bookmark at 20"
        );
    }

    // ─── Button clicks (real egui hit-test) ──────────────────────────────

    #[test]
    fn copy_event_copies_selected_row() {
        // egui/winit deliver Ctrl/Cmd+C as Event::Copy, not a Key::C press.
        // Simulate the real event and verify the row-copy path runs — status
        // reflects a copy attempt ("copied" on success, or a headless clipboard
        // error), never left blank as it was when we matched the wrong key.
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 10);
        h.state_mut().select_filtered_row_with_len(3, 10);
        h.run();
        h.state_mut().status.clear();

        h.event(egui::Event::Copy);
        h.run();

        assert!(
            !h.state().status.is_empty(),
            "Event::Copy with a row selected should run the copy path"
        );
    }

    #[test]
    fn click_clear_button_empties_model() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 50);
        h.run();

        assert!(!h.state().model.read().unwrap().entries.is_empty());

        // Click the "Clear" button in the toolbar
        let label = tr!("clear");
        h.get_by_label(label.as_str()).click();
        h.run();

        assert!(
            h.state().model.read().unwrap().entries.is_empty(),
            "clicking Clear should empty the model"
        );
    }

    #[test]
    fn status_auto_expires_after_ttl() {
        let mut h = harness();
        h.run();
        h.state_mut().status = "已复制 1 行".into();

        // First tick records when the message appeared.
        h.state_mut().tick_status(100.0);
        assert_eq!(h.state().status, "已复制 1 行");
        // Still within the 5s TTL — kept.
        h.state_mut().tick_status(104.0);
        assert_eq!(h.state().status, "已复制 1 行");
        // Past the TTL — cleared so "Selected N" can show again.
        h.state_mut().tick_status(106.0);
        assert_eq!(h.state().status, "");

        // A new message resets the timer (doesn't inherit the old expiry).
        h.state_mut().status = "已保存".into();
        h.state_mut().tick_status(106.0);
        h.state_mut().tick_status(108.0);
        assert_eq!(
            h.state().status,
            "已保存",
            "new message should not expire early"
        );
    }

    #[test]
    fn copy_row_respects_visible_columns() {
        // Exercises the real App::copy_selected_rows_text / visible_row_text.
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 3); // pid 100, tid 200, tag Tag{i%3}, msg "msg {i}"
        h.state_mut().selected_rows.insert(0);
        h.state_mut().selected_rows.insert(2);

        let text = h.state().copy_selected_rows_text();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 2, "rows 0 and 2 → 2 lines, got {lines:?}");
        assert!(lines[0].contains("msg 0"), "first line: {:?}", lines[0]);
        assert!(lines[1].contains("msg 2"), "second line: {:?}", lines[1]);
        // UID column is hidden by default → no phantom empty field.
        assert!(
            !lines[0].contains("\t\t"),
            "hidden UID must not leave an empty column: {:?}",
            lines[0]
        );

        // Hiding a column drops it from the copy (WYSIWYG).
        h.state_mut().ui.col_message = false;
        let text2 = h.state().copy_selected_rows_text();
        assert!(
            !text2.contains("msg"),
            "hidden Message column must not be copied: {text2:?}"
        );
    }

    #[test]
    fn clear_in_file_mode_bumps_epoch_to_stop_tail() {
        // With no device session, Clear must bump the source epoch so the file-tail
        // poller stops feeding the just-emptied view. (The adb-live branch, which
        // must NOT bump so the live stream survives, needs a real Session and is
        // covered by review rather than a unit test.)
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 10);
        assert!(h.state().session.is_none());
        let before = h.state().source_epoch.load(Ordering::Acquire);
        h.state_mut().clear();
        let after = h.state().source_epoch.load(Ordering::Acquire);
        assert!(
            after > before,
            "file-mode Clear should bump the epoch (was {before}, now {after})"
        );
    }

    #[test]
    fn click_run_no_panic_and_status_updates() {
        let mut h = harness();
        h.run();

        // Click "Run" — on CI (no adb): fails gracefully with error status.
        // On dev machines (adb present): starts a session. Either way: no panic,
        // and status is set to something non-empty.
        let label = tr!("run");
        h.get_by_label(label.as_str()).click();
        h.run();

        assert!(
            !h.state().status.is_empty(),
            "clicking Run should update status (success or error)"
        );
    }

    // ─── FilterSpec sync ─────────────────────────────────────────────────

    #[test]
    fn filter_spec_reflects_ui_state_after_notify() {
        let mut h = harness();
        h.run();

        let app = h.state_mut();
        app.ui.find = "hello".into();
        app.ui.find_on = true;
        app.ui.remove = "spam".into();
        app.ui.remove_on = true;
        app.ui.bookmarks_only = true;
        app.notify_filter();

        let spec = app.shared_filter.read().unwrap();
        assert_eq!(spec.find, vec!["hello"]);
        assert_eq!(spec.remove, vec!["spam"]);
        assert!(spec.bookmarks_only);
    }

    #[test]
    fn disallowed_tags_propagates_to_spec() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 10);
        h.run();

        h.state_mut().add_remove_tag("BadTag");

        let spec = h.state().shared_filter.read().unwrap();
        assert!(spec.disallowed_tags.contains("BadTag"));
        assert!(h.state().ui.allowed_tags.is_none());
    }

    // ─── Render with data (smoke) ────────────────────────────────────────

    #[test]
    fn render_200_entries_with_filters_no_panic() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 200);
        h.state_mut().ui.find = "msg 1".into();
        h.state_mut().ui.find_on = true;
        h.state_mut().ui.highlight = "Tag".into();
        h.state_mut().ui.highlight_on = true;
        h.state_mut().notify_filter();
        // Drive multiple frames with active filters + highlights
        h.run();
        h.run();
        h.run();
    }

    #[test]
    fn all_rows_filtered_out_renders_no_matches_hint_without_panic() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.run();
        // A Find term that matches nothing hides every row.
        h.state_mut().ui.find = "zzz_no_such_token_zzz".into();
        h.state_mut().ui.find_on = true;
        h.state_mut().notify_filter();
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.run(); // renders the no-matches hint (must not panic)

        let m = h.state().model.read().unwrap();
        assert!(!m.entries.is_empty(), "entries stay loaded");
        assert!(
            m.filtered.is_empty(),
            "a non-matching Find should hide every row, got {} visible",
            m.filtered.len()
        );
    }

    #[test]
    fn f2_jumps_to_previous_bookmark() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 30);
        h.state_mut().toggle_bookmark(5);
        h.state_mut().toggle_bookmark(20);
        h.state_mut().select_filtered_row_with_len(25, 30);
        h.run();

        // F2 = previous bookmark
        h.key_press(egui::Key::F2);
        h.run();
        assert!(
            h.state().selected_rows.contains(&20),
            "F2 should jump back to bookmark 20"
        );

        h.key_press(egui::Key::F2);
        h.run();
        assert!(
            h.state().selected_rows.contains(&5),
            "F2 again should jump back to bookmark 5"
        );
    }

    #[test]
    fn page_down_key_jumps_forward() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 200);
        h.state_mut().select_filtered_row_with_len(0, 200);
        h.run(); // let the table render and set visible_table_rows

        h.key_press(egui::Key::PageDown);
        h.run();

        let pos = *h.state().selected_rows.iter().next().unwrap();
        // Should jump forward by at least 1 row (actual page size depends on harness layout)
        assert!(pos > 0, "PageDown should move forward from 0, got {pos}");
    }

    #[test]
    fn page_down_pages_from_cursor_not_arbitrary_selection() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 300);
        // Selection member at row 0, but the cursor is far away at 100. Paging
        // must follow the cursor (deterministic), not the HashSet member.
        h.state_mut().selected_rows.clear();
        h.state_mut().selected_rows.insert(0);
        h.state_mut().selection_cursor = Some(100);
        h.state_mut().selection_anchor = Some(100);
        h.run(); // set visible_table_rows

        h.key_press(egui::Key::PageDown);
        h.run();

        let pos = *h.state().selected_rows.iter().next().unwrap();
        assert!(
            pos > 100,
            "PageDown must page from the cursor (100), not the selection member (0); got {pos}"
        );
    }

    #[test]
    fn f3_bookmark_jump_starts_from_cursor_not_arbitrary_selection() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 300);
        h.state_mut().toggle_bookmark(50);
        h.state_mut().toggle_bookmark(150);
        // Selection member at row 0, cursor at 100. Forward jump from the cursor
        // should reach bookmark 150 — not 50 (which is what a jump from row 0 gives).
        h.state_mut().selected_rows.clear();
        h.state_mut().selected_rows.insert(0);
        h.state_mut().selection_cursor = Some(100);
        h.state_mut().selection_anchor = Some(100);
        h.run();

        h.key_press(egui::Key::F3);
        h.run();

        assert!(
            h.state().selected_rows.contains(&150),
            "F3 should jump from the cursor (100) to bookmark 150, got {:?}",
            h.state().selected_rows
        );
    }

    #[test]
    fn editing_highlight_keeps_selection() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.state_mut().select_filtered_row_with_len(5, 20);
        h.run();
        assert!(h.state().selected_rows.contains(&5));

        // Simulate the user typing into the Highlight field. Highlight is visual
        // only, so it must not clear the row selection (which notify_filter does).
        h.ctx
            .memory_mut(|m| m.request_focus(egui::Id::new("filter_highlight_edit")));
        h.run();
        h.event(egui::Event::Text("err".into()));
        h.run();

        assert_eq!(
            h.state().ui.highlight,
            "err",
            "highlight text should update"
        );
        assert!(
            h.state().selected_rows.contains(&5),
            "editing Highlight must not clear the row selection"
        );
    }

    #[test]
    fn page_up_key_jumps_back() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 200);
        h.state_mut().select_filtered_row_with_len(100, 200);
        h.run();

        h.key_press(egui::Key::PageUp);
        h.run();

        let pos = *h.state().selected_rows.iter().next().unwrap();
        assert!(pos < 100, "PageUp from 100 should move backward, got {pos}");
    }

    #[test]
    fn ctrl_plus_minus_changes_font_size() {
        let mut h = harness();
        h.run();
        let initial = h.state().cfg.view.font_size;

        // Ctrl+= (plus) should increase
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Equals);
        h.run();
        assert!(
            h.state().cfg.view.font_size > initial,
            "Ctrl+= should increase font size from {initial}, got {}",
            h.state().cfg.view.font_size
        );

        // Ctrl+- should decrease
        let before_minus = h.state().cfg.view.font_size;
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Minus);
        h.run();
        assert!(
            h.state().cfg.view.font_size < before_minus,
            "Ctrl+- should decrease font size from {before_minus}, got {}",
            h.state().cfg.view.font_size
        );
    }

    #[test]
    fn ctrl_zero_resets_font_size() {
        let mut h = harness();
        h.run();
        // Increase font first
        h.state_mut().cfg.view.font_size = 17.0;
        h.run();

        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Num0);
        h.run();

        assert_eq!(
            h.state().cfg.view.font_size,
            13.0,
            "Ctrl+0 should reset to default 13.0"
        );
    }

    // ─── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn arrow_down_on_empty_table_no_panic() {
        let mut h = harness();
        h.run();
        // No data injected — table is empty
        h.key_press(egui::Key::ArrowDown);
        h.run();
        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowDown);
        h.run();
        // No panic = success
        assert!(h.state().selected_rows.is_empty());
    }

    #[test]
    fn shift_arrow_direction_change_shrinks_range() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 30);
        h.state_mut().select_filtered_row_with_len(15, 30);
        h.run();

        // Extend down 3
        for _ in 0..3 {
            h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowDown);
            h.run();
        }
        assert_eq!(h.state().selected_rows.len(), 4); // 15..=18

        // Now reverse: up 2 — range should shrink to 15..=16
        for _ in 0..2 {
            h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowUp);
            h.run();
        }
        assert_eq!(h.state().selected_rows.len(), 2); // 15..=16
        assert_eq!(h.state().selection_cursor, Some(16));
    }

    #[test]
    fn rapid_arrow_down_moves_multiple_rows() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 50);
        h.state_mut().select_filtered_row_with_len(0, 50);
        h.run();

        for _ in 0..10 {
            h.key_press(egui::Key::ArrowDown);
            h.run();
        }
        assert!(h.state().selected_rows.contains(&10));
    }

    // ─── adb Run / Pause / Stop lifecycle ────────────────────────────────

    #[test]
    fn run_session_clears_model_and_selection() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 50);
        h.state_mut().select_filtered_row_with_len(10, 50);
        h.run();

        assert!(!h.state().model.read().unwrap().entries.is_empty());
        assert!(!h.state().selected_rows.is_empty());

        h.state_mut().run_session();
        // Regardless of whether adb spawned, model and selection must be cleared
        assert!(h.state().model.read().unwrap().entries.is_empty());
        assert!(h.state().selected_rows.is_empty());
        assert_eq!(h.state().selection_anchor, None);
    }

    #[test]
    fn run_session_preserves_column_filters() {
        use crate::model::LevelMask;
        let mut h = harness();
        h.run();

        // User narrows the view by level/PID/TID/tag before restarting capture.
        h.state_mut().ui.allowed_levels = Some(LevelMask::E);
        h.state_mut().ui.allowed_pids = Some(std::collections::HashSet::from(["100".to_string()]));
        h.state_mut().ui.allowed_tids = Some(std::collections::HashSet::from(["200".to_string()]));
        h.state_mut().ui.allowed_tags = Some(std::collections::HashSet::from(["Tag0".to_string()]));
        h.state_mut().ui.disallowed_tags.insert("Tag1".to_string());

        // Run/Restart re-monitors the same source: filters must survive.
        h.state_mut().run_session();

        assert_eq!(h.state().ui.allowed_levels, Some(LevelMask::E));
        assert_eq!(
            h.state().ui.allowed_pids,
            Some(std::collections::HashSet::from(["100".to_string()]))
        );
        assert_eq!(
            h.state().ui.allowed_tids,
            Some(std::collections::HashSet::from(["200".to_string()]))
        );
        assert_eq!(
            h.state().ui.allowed_tags,
            Some(std::collections::HashSet::from(["Tag0".to_string()]))
        );
        assert!(h.state().ui.disallowed_tags.contains("Tag1"));
    }

    #[test]
    fn stop_session_when_no_session_is_noop() {
        let mut h = harness();
        h.run();
        assert!(h.state().session.is_none());
        h.state_mut().stop_session();
        // No panic, status unchanged
    }

    #[test]
    fn toggle_pause_when_no_session_is_noop() {
        let mut h = harness();
        h.run();
        assert!(h.state().session.is_none());
        h.state_mut().toggle_pause();
        // No panic
    }

    // ─── Open local file ─────────────────────────────────────────────────

    #[test]
    fn open_file_loads_entries_into_model() {
        let mut h = harness();
        h.run();

        // Write a small threadtime logcat file
        let tmp = std::env::temp_dir().join(format!("lf_open_{}.log", std::process::id()));
        std::fs::write(
            &tmp,
            "\
01-01 10:00:00.000  100  200 I Tag1: first line\n\
01-01 10:00:01.000  100  200 W Tag2: second line\n\
01-01 10:00:02.000  100  200 E Tag1: third line\n\
",
        )
        .unwrap();

        let result = h.state_mut().open_file(&tmp);
        assert!(
            result.is_ok(),
            "open_file should succeed: {:?}",
            result.err()
        );

        // Give the ingest thread time to process
        std::thread::sleep(std::time::Duration::from_millis(100));
        h.run();

        let m = h.state().model.read().unwrap();
        assert_eq!(
            m.entries.len(),
            3,
            "should have 3 entries, got {}",
            m.entries.len()
        );
        assert_eq!(m.file_path.as_ref().unwrap(), &tmp);
        assert_eq!(m.entries[0].tag(), "Tag1");
        assert_eq!(m.entries[1].tag(), "Tag2");
        assert_eq!(m.entries[2].level, crate::model::LevelMask::E);
        drop(m);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn open_file_populates_detected_encoding() {
        let mut h = harness();
        h.run();
        let tmp = std::env::temp_dir().join(format!("lf_enc_disp_{}.log", std::process::id()));
        std::fs::write(&tmp, "01-01 10:00:00.000  1  2 I T: 中文 abc\n").unwrap();
        h.state_mut().open_file(&tmp).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(120));
        h.run();
        let enc = h.state().detected_encoding.lock_recover().clone();
        assert_eq!(
            enc.as_deref(),
            Some("UTF-8"),
            "status bar should reflect the actually-used encoding"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn toggling_auto_scroll_on_runs_jump_path_without_panic() {
        // Turning Auto-scroll ON requests a jump to the bottom (pending_scroll),
        // which the table then consumes within the same frame — so it can't be
        // observed after run(). This exercises the toggle path end-to-end and
        // guards it against panics (e.g. the model read / empty-filtered guard).
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 50);
        h.state_mut().auto_scroll = false;
        h.run();

        let label = tr!("auto_scroll");
        h.get_by_label(label.as_str()).click();
        h.run();
        assert!(h.state().auto_scroll, "clicking should turn Auto-scroll on");
    }

    #[test]
    fn open_file_tails_appended_lines_without_moving_scroll() {
        let mut h = harness();
        h.run();

        let tmp = std::env::temp_dir().join(format!("lf_tail_e2e_{}.log", std::process::id()));
        std::fs::write(
            &tmp,
            "01-01 10:00:00.000  100  200 I Tag1: first\n\
             01-01 10:00:01.000  100  200 W Tag2: second\n",
        )
        .unwrap();

        h.state_mut().open_file(&tmp).unwrap();
        // Poll for the initial 2 lines rather than a fixed sleep (flakes on CI).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while h.state().model.read().unwrap().entries.len() != 2
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
            h.run();
        }
        assert_eq!(h.state().model.read().unwrap().entries.len(), 2);
        // Tailing must not hijack the viewport.
        assert_eq!(
            h.state().pending_scroll,
            None,
            "tail must not force a scroll"
        );

        // Append two more lines to the file on disk.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(b"01-01 10:00:02.000  100  200 E Tag1: third\n")
                .unwrap();
            f.write_all(b"01-01 10:00:03.000  100  200 I Tag3: fourth\n")
                .unwrap();
        }

        // Poll until the appended lines are tailed in (500ms poll + ingest).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while h.state().model.read().unwrap().entries.len() != 4
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
            h.run();
        }

        let m = h.state().model.read().unwrap();
        assert_eq!(
            m.entries.len(),
            4,
            "appended lines should be tailed in, got {}",
            m.entries.len()
        );
        assert_eq!(m.entries[3].tag(), "Tag3");
        drop(m);
        assert_eq!(
            h.state().pending_scroll,
            None,
            "tail still must not move scroll"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn open_file_reloads_on_truncation() {
        let mut h = harness();
        h.run();

        let tmp = std::env::temp_dir().join(format!("lf_tail_rot_{}.log", std::process::id()));
        std::fs::write(
            &tmp,
            "01-01 10:00:00.000  100  200 I Tag1: alpha\n\
             01-01 10:00:01.000  100  200 I Tag1: beta\n\
             01-01 10:00:02.000  100  200 I Tag1: gamma\n",
        )
        .unwrap();
        h.state_mut().open_file(&tmp).unwrap();

        // Poll for the initial 3 lines to ingest rather than assuming a fixed
        // delay — the tail poll (500ms) + ingest thread run at their own pace, so
        // fixed sleeps flake on slow/loaded CI.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while h.state().model.read().unwrap().entries.len() != 3
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
            h.run();
        }
        assert_eq!(
            h.state().model.read().unwrap().entries.len(),
            3,
            "initial load should ingest 3 lines"
        );

        // Rotate: replace with a shorter file (fewer bytes than already read).
        std::fs::write(&tmp, "01-01 10:10:00.000  100  200 W Tag9: rotated\n").unwrap();

        // Poll until the rotation is picked up: tail poll → reload_request →
        // reload → ingest the new, shorter file.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            {
                let m = h.state().model.read().unwrap();
                if m.entries.len() == 1 && m.entries[0].tag() == "Tag9" {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rotation was not reflected in time; entries = {}",
                h.state().model.read().unwrap().entries.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
            h.run();
        }

        let m = h.state().model.read().unwrap();
        assert_eq!(
            m.entries.len(),
            1,
            "after rotation the model reflects the new, shorter file"
        );
        assert_eq!(m.entries[0].tag(), "Tag9");
        drop(m);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn open_file_nonexistent_returns_error() {
        let mut h = harness();
        h.run();

        let result = h.state_mut().open_file(std::path::Path::new(
            "/tmp/nonexistent_logfilter_test_xyz.log",
        ));
        assert!(result.is_err(), "open_file on nonexistent path should fail");
    }

    #[test]
    fn open_file_clears_previous_data() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 50);
        h.state_mut().toggle_bookmark(10);
        h.state_mut().select_filtered_row_with_len(20, 50);
        h.run();

        // Write a new file and open it
        let tmp = std::env::temp_dir().join(format!("lf_open2_{}.log", std::process::id()));
        std::fs::write(&tmp, "01-01 10:00:00.000  1  1 I T: msg\n").unwrap();

        h.state_mut().open_file(&tmp).unwrap();

        // Previous selection and bookmarks must be gone
        assert!(h.state().selected_rows.is_empty());
        assert_eq!(h.state().selection_anchor, None);
        // Model was cleared (old entries gone; new file loading in background)
        let m = h.state().model.read().unwrap();
        assert!(m.bookmarks.is_empty());
        drop(m);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn open_different_file_resets_column_filters() {
        use crate::model::LevelMask;
        let mut h = harness();
        h.run();

        let a = std::env::temp_dir().join(format!("lf_srcA_{}.log", std::process::id()));
        std::fs::write(&a, "01-01 10:00:00.000  1  1 I T: msg\n").unwrap();
        h.state_mut().open_file(&a).unwrap();

        // Filters set against file A, then a *different* file B is opened.
        h.state_mut().ui.allowed_levels = Some(LevelMask::E);
        h.state_mut().ui.allowed_pids = Some(std::collections::HashSet::from(["1".to_string()]));

        let b = std::env::temp_dir().join(format!("lf_srcB_{}.log", std::process::id()));
        std::fs::write(&b, "02-02 11:00:00.000  9  9 W T: other\n").unwrap();
        h.state_mut().open_file(&b).unwrap();

        // Unrelated values would hide the new file's data — must be cleared.
        assert_eq!(h.state().ui.allowed_levels, None);
        assert_eq!(h.state().ui.allowed_pids, None);

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn reopen_same_file_keeps_column_filters() {
        use crate::model::LevelMask;
        let mut h = harness();
        h.run();

        let p = std::env::temp_dir().join(format!("lf_same_{}.log", std::process::id()));
        std::fs::write(&p, "01-01 10:00:00.000  1  1 I T: msg\n").unwrap();
        h.state_mut().open_file(&p).unwrap();

        h.state_mut().ui.allowed_levels = Some(LevelMask::I);
        h.state_mut().ui.allowed_tags = Some(std::collections::HashSet::from(["T".to_string()]));

        // Reloading the same path (mirrors a truncation/rotation reload) keeps
        // filters, since the values still refer to the same source.
        h.state_mut().open_file(&p).unwrap();

        assert_eq!(h.state().ui.allowed_levels, Some(LevelMask::I));
        assert_eq!(
            h.state().ui.allowed_tags,
            Some(std::collections::HashSet::from(["T".to_string()]))
        );

        let _ = std::fs::remove_file(&p);
    }

    // ─── Bookmarks + filter interaction ──────────────────────────────────

    #[test]
    fn bookmark_toggle_with_bookmarks_only_active() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.run();

        // Enable bookmarks_only
        h.state_mut().ui.bookmarks_only = true;
        h.state_mut().notify_filter();
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.run();

        // Toggle bookmark — should not panic regardless of bookmarks_only state
        h.state_mut().toggle_bookmark(5);
        assert!(h.state().model.read().unwrap().bookmarks.contains(&5));

        // Toggle again to remove
        h.state_mut().toggle_bookmark(5);
        assert!(!h.state().model.read().unwrap().bookmarks.contains(&5));
    }

    #[test]
    fn goto_visible_row_selects_and_scrolls() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20); // filtered = 0..20, all visible
        h.run();

        h.state_mut().goto_line(7); // user typed line 8
        assert!(h.state().selected_rows.contains(&7));
        assert_eq!(h.state().pending_scroll, Some(7));
        assert_eq!(h.state().selection_cursor, Some(7));
        assert!(h.state().status.is_empty(), "no error status on a hit");
    }

    #[test]
    fn goto_out_of_range_reports_and_does_not_select() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        h.run();

        h.state_mut().goto_line(999);
        assert!(
            h.state().selected_rows.is_empty(),
            "out-of-range goto must not select"
        );
        assert_eq!(h.state().pending_scroll, None);
        assert!(!h.state().status.is_empty(), "should report out of range");
    }

    #[test]
    fn goto_filtered_row_reports_and_scrolls_to_nearest() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        // Hide odd rows: filtered keeps only even entry indices (0,2,4,...,18).
        {
            let mut m = h.state().model.write().unwrap();
            m.filtered = (0..20u32).filter(|i| i % 2 == 0).collect();
        }
        h.run();

        // Line 10 (row index 9) is odd → hidden. Nearest visible by
        // partition_point is entry 10, which sits at position 5 in `filtered`.
        h.state_mut().goto_line(9);
        assert!(
            h.state().selected_rows.is_empty(),
            "hidden goto must not select"
        );
        assert!(
            !h.state().status.is_empty(),
            "should report filtered-hidden"
        );
        assert_eq!(
            h.state().pending_scroll,
            Some(5),
            "should scroll to nearest visible row"
        );
    }

    #[test]
    fn goto_filtered_with_empty_filtered_no_panic() {
        let mut h = harness();
        h.run();
        inject(h.state_mut(), 20);
        {
            let mut m = h.state().model.write().unwrap();
            m.filtered.clear(); // nothing visible
        }
        h.run();

        h.state_mut().goto_line(5); // exists but nothing is visible
        assert_eq!(h.state().pending_scroll, None);
        assert!(!h.state().status.is_empty());
    }
}
