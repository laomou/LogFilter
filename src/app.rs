use crate::adb;
use crate::config::{self, parse_color, Config};
use crate::filter::FilterSpec;
use crate::io::{send_decoded_lines, send_utf8_lines};
use crate::model::{EncodingChoice, LevelMask, Model};
use crate::parser::parse_line;
use crate::fonts::{bump_global_text_sizes, install_ui_font, list_user_font_stems};
use anyhow::Result;
use egui_i18n::tr;
use crossbeam_channel::{bounded, Receiver, Sender};
use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};
use egui_extras::{Column, TableBuilder};
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
    /// Monotonic id of the current line source (file load or adb session). Each
    /// new load/adb-run bumps it; the ingest thread drops any queued line whose
    /// epoch != this, so a superseded load can't interleave into the new one.
    pub source_epoch: Arc<AtomicU64>,
    pub wake: Arc<(Mutex<bool>, Condvar)>,
    pub status: String,
    pub ui: UiState,

    pub selected_rows: HashSet<usize>,
    /// Anchor row for Shift+Arrow / Shift+Click range selection.
    pub selection_anchor: Option<usize>,
    pub pending_scroll: Option<usize>,
    pub visible_table_rows: usize,
    /// Window inner size captured each frame, saved on exit.
    last_window_size: Option<egui::Vec2>,

    // All font stems found in config/fonts — just metadata, no bytes loaded.
    // Populated once at startup via list_user_font_stems().
    pub user_font_stems: Vec<(String, String)>,

    // adb
    pub line_tx: Sender<(u64, String)>,
    pub adb_session: Option<adb::Session>,
    pub adb_devices: Vec<String>,
    pub selected_device: String,
    pub selected_cmd: String,
    pub auto_scroll: bool,

    // Per-frame caches: recomputed lazily when source data changes.
    cached_highlight_palette: Vec<Color32>,
    cached_highlight_tokens: Vec<String>,
    cached_find_tokens: Vec<String>,
    /// Raw highlight/find strings that were used to produce the token caches above.
    cached_highlight_raw: String,
    cached_find_raw: String,
    /// Cached shortcut-rows for the empty-table view. Invalidated on language switch.
    cached_shortcut_rows: Vec<EmptyShortcutRow>,

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
    pub col_tag: bool,
    pub col_message: bool,

    pub goto_line: String,

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
    fn from_config(cfg: &Config) -> Self {
        Self {
            find: cfg.filters.find.clone(),
            find_on: !cfg.filters.find.is_empty(),
            remove: cfg.filters.remove.clone(),
            remove_on: !cfg.filters.remove.is_empty(),
            highlight: cfg.filters.highlight.clone(),
            highlight_on: !cfg.filters.highlight.is_empty(),
            allowed_pids: None,
            allowed_tids: None,
            allowed_tags: None,
            allowed_levels: None,
            encoding: cfg.view.encoding.clone(),
            col_bookmark: false,
            col_line: true,
            col_date: true,
            col_time: true,
            col_loglv: true,
            col_pid: true,
            col_thread: true,
            col_tag: true,
            col_message: true,
            goto_line: String::new(),
            picker: None,
        }
    }

    fn to_filter_spec(&self) -> FilterSpec {
        FilterSpec {
            allowed_levels: self.allowed_levels,
            allowed_pids: self.allowed_pids.clone(),
            allowed_tids: self.allowed_tids.clone(),
            allowed_tags: self.allowed_tags.clone(),
            find: if self.find_on { FilterSpec::tokens(&self.find) } else { vec![] },
            remove: if self.remove_on { FilterSpec::tokens(&self.remove) } else { vec![] },
            bookmarks_only: false,
            errors_only: false,
        }
    }

    fn write_back(&self, cfg: &mut Config) {
        cfg.filters.find = self.find.clone();
        cfg.filters.remove = self.remove.clone();
        cfg.filters.highlight = self.highlight.clone();
        cfg.view.encoding = self.encoding.clone();
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        tune_table_visuals(&cc.egui_ctx);
        let cfg = config::load();
        init_i18n();
        // Apply the stored language (or auto-detect) at startup.
        resolve_startup_lang(&cfg.view.lang);
        let font_stems = list_user_font_stems();
        install_ui_font(&cc.egui_ctx, &cfg.view.font, &font_stems);
        bump_global_text_sizes(&cc.egui_ctx);
        // egui defaults Ctrl+= / Ctrl+- / Ctrl+0 to changing the global zoom_factor,
        // which scales the entire UI (menus, toolbar, table). We only want those
        // shortcuts to change the table font size, so disable egui's handler and
        // implement our own in `update()`.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let ui = UiState::from_config(&cfg);
        let shared_filter = Arc::new(RwLock::new(ui.to_filter_spec()));
        // Bounded so a fast file reader can't buffer the whole file as queued
        // Strings ahead of the (slower) parse/append step — it blocks instead,
        // capping peak memory. 8192 ≈ a few ingest batches of headroom.
        let (line_tx, line_rx) = bounded::<(u64, String)>(8192);
        let selected_cmd = cfg.adb.commands.first().cloned().unwrap_or_else(|| "logcat -v threadtime".into());
        // Prime caches from the initial config so the first frame doesn't reallocate.
        let init_hl_raw = if ui.highlight_on { ui.highlight.clone() } else { String::new() };
        let init_find_raw = if ui.find_on { ui.find.clone() } else { String::new() };
        let init_palette: Vec<Color32> = cfg.colors.highlights.iter().map(|s| parse_color(s)).collect();
        let mut app = Self {
            cfg,
            model: Arc::new(RwLock::new(Model::default())),
            shared_filter,
            gen: Arc::new(AtomicU64::new(0)),
            source_epoch: Arc::new(AtomicU64::new(0)),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            status: String::new(),
            ui,
            selected_rows: HashSet::new(),
            selection_anchor: None,
            pending_scroll: None,
            visible_table_rows: 1,
            last_window_size: None,
            user_font_stems: font_stems,
            line_tx,
            adb_session: None,
            adb_devices: Vec::new(),
            selected_device: String::new(),
            selected_cmd,
            auto_scroll: true,
            cached_highlight_palette: init_palette,
            cached_highlight_tokens: if init_hl_raw.is_empty() { vec![] } else { FilterSpec::tokens(&init_hl_raw) },
            cached_find_tokens: if init_find_raw.is_empty() { vec![] } else { FilterSpec::tokens(&init_find_raw) },
            cached_highlight_raw: init_hl_raw,
            cached_find_raw: init_find_raw,
            cached_shortcut_rows: empty_shortcut_rows(),
        };
        app.spawn_filter_thread(cc.egui_ctx.clone());
        app.spawn_ingest_thread(cc.egui_ctx.clone(), line_rx);
        // Pre-populate the device combo on startup so the user doesn't have to
        // click ↻ once before they can pick a device.
        app.refresh_devices();
        if let Some(path) = initial_file {
            if let Err(e) = app.open_file(&path) {
                app.status = format!("Failed to open {}: {}", path.display(), e);
            }
        }
        app.notify_filter();
        app
    }

    pub fn open_file(&mut self, path: &Path) -> Result<()> {
        // Validate access synchronously so the common errors (missing file, no
        // permission) are still reported inline; the heavy read+decode+parse is
        // moved off the UI thread below.
        let _ = std::fs::File::open(path)?;

        // Stop any adb session so its lines don't interleave with the file.
        self.adb_stop();

        // Claim a fresh source epoch *before* clearing so any lines still queued
        // from a previous load/adb are dropped by the ingest thread.
        let epoch = self.source_epoch.fetch_add(1, Ordering::AcqRel) + 1;

        // Reset the model synchronously (cheap) and let lines stream in.
        {
            let mut model = self.model.write().unwrap();
            model.clear();
            model.file_path = Some(path.to_path_buf());
        }
        self.selected_rows.clear();
        config::add_recent(&mut self.cfg, path);
        self.notify_filter();

        // Background reader: read + decode, then feed lines through the existing
        // ingest channel so parsing/appending/repaint happen incrementally on
        // the ingest thread — the UI stays responsive for large files. The
        // reader bails out early if a newer load supersedes this epoch.
        let tx = self.line_tx.clone();
        let source_epoch = self.source_epoch.clone();
        let choice = self.encoding_choice();
        let path = path.to_path_buf();
        thread::Builder::new()
            .name("file-load".into())
            .spawn(move || match choice {
                EncodingChoice::Utf8 => send_utf8_lines(&path, tx, epoch, source_epoch),
                EncodingChoice::Local => send_decoded_lines(&path, tx, epoch, source_epoch, choice),
            })?;
        Ok(())
    }

    fn encoding_choice(&self) -> EncodingChoice {
        match self.ui.encoding.as_str() {
            "local" => EncodingChoice::Local,
            _ => EncodingChoice::Utf8,
        }
    }

    fn notify_filter(&self) {
        *self.shared_filter.write().unwrap() = self.ui.to_filter_spec();
        self.gen.fetch_add(1, Ordering::AcqRel);
        let (lock, cvar) = &*self.wake;
        *lock.lock().unwrap() = true;
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
                let loc = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
                if loc.starts_with("zh") { "zh-CN" } else { "en-US" }
            }
        };
        egui_i18n::set_language(code);
        // Rebuild: shortcut-row strings embed translated labels.
        self.cached_shortcut_rows = empty_shortcut_rows();
    }
    fn spawn_ingest_thread(&self, ctx: egui::Context, rx: Receiver<(u64, String)>) {
        let model = self.model.clone();
        let wake = self.wake.clone();
        let source_epoch = self.source_epoch.clone();
        thread::Builder::new().name("ingest".into()).spawn(move || {
            let mut batch: Vec<(u64, String)> = Vec::with_capacity(256);
            loop {
                // block for first line
                let Ok(first) = rx.recv() else { return; };
                batch.clear();
                batch.push(first);
                // drain more if available (up to 512 lines / 25ms)
                let deadline = std::time::Instant::now() + Duration::from_millis(25);
                while batch.len() < 512 {
                    let remain = deadline.saturating_duration_since(std::time::Instant::now());
                    if remain.is_zero() { break; }
                    match rx.recv_timeout(remain) {
                        Ok(l) => batch.push(l),
                        Err(_) => break,
                    }
                }
                // Drop lines from a superseded source (an older file load / adb
                // session) so they never interleave into the current one.
                let cur = source_epoch.load(Ordering::Acquire);
                let mut appended = false;
                {
                    let mut m = model.write().unwrap();
                    for (ep, line) in batch.drain(..) {
                        if ep != cur { continue; }
                        let (entry, _) = parse_line(line);
                        m.append(entry);
                        appended = true;
                    }
                }
                if appended {
                    // Wake the filter thread for an append-only pass. Deliberately
                    // do NOT bump `gen` — that's reserved for filter-spec changes,
                    // which force a full recompute (see spawn_filter_thread).
                    let (lock, cvar) = &*wake;
                    *lock.lock().unwrap() = true;
                    cvar.notify_one();
                    ctx.request_repaint();
                }
            }
        }).expect("spawn ingest thread");
    }

    fn adb_run(&mut self) {
        self.adb_stop();
        let epoch = self.source_epoch.fetch_add(1, Ordering::AcqRel) + 1;

        // A fresh run starts from an empty table — clear any entries left from a
        // previous run/file (mirrors the file-load path). The epoch bump above
        // already ensures stale queued lines are dropped by the ingest thread.
        {
            let mut model = self.model.write().unwrap();
            model.clear();
        }
        self.selected_rows.clear();
        self.notify_filter();

        let device = if self.selected_device.is_empty() { None } else { Some(self.selected_device.as_str()) };
        match adb::Session::start(
            self.cfg.adb.adb_path.as_deref(),
            device,
            &self.selected_cmd,
            self.line_tx.clone(),
            epoch,
        ) {
            Ok(s) => {
                self.adb_session = Some(s);
                self.status = tr!("status_adb_started", { cmd: &self.selected_cmd });
            }
            Err(e) => {
                self.status = tr!("status_adb_start_failed", { e: &format!("{}", e) });
            }
        }
    }

    fn adb_stop(&mut self) {
        if let Some(mut s) = self.adb_session.take() {
            s.stop();
            self.status = tr!("status_adb_stopped");
        }
    }

    fn adb_pause_toggle(&mut self) {
        if let Some(s) = &self.adb_session {
            let new = !s.is_paused();
            s.set_paused(new);
            self.status = if new { tr!("status_adb_paused") } else { tr!("status_adb_resumed") };
        }
    }

    fn clear(&mut self) {
        {
            let mut m = self.model.write().unwrap();
            m.clear();
        }
        self.selected_rows.clear();
        self.notify_filter();
    }

    fn copy_selected_rows_text(&self) -> String {
        let m = self.model.read().unwrap();
        let mut rows: Vec<&usize> = self.selected_rows.iter().collect();
        rows.sort();
        let texts: Vec<String> = rows.iter().filter_map(|&&r| {
            let &ei = m.filtered.get(r)?;
            let e = &m.entries[ei as usize];
            Some(format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                e.line_no, e.date(), e.time(), e.level.as_char(), e.pid(), e.tid(), e.tag(), e.message()
            ))
        }).collect();
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
        let texts: Vec<&str> = rows.iter().filter_map(|&&r| {
            let &ei = filtered.get(r)?;
            Some(col(&entries[ei as usize]))
        }).collect();
        texts.join("\n")
    }

    fn copy_selected_row(&mut self) {
        if self.selected_rows.is_empty() { return; }
        let text = self.copy_selected_rows_text();
        let n = text.lines().count();
        match arboard::Clipboard::new() {
            Ok(mut c) => {
                if let Err(e) = c.set_text(&text) {
                    self.status = format!("Clipboard error: {e}");
                } else {
                    self.status = if n > 1 {
                        format!("Copied {n} rows")
                    } else {
                        "Copied 1 row".into()
                    };
                }
            }
            Err(e) => self.status = format!("Clipboard error: {e}"),
        }
    }

    fn save_filtered(&mut self) {
        let m = self.model.read().unwrap();
        if m.filtered.is_empty() && m.entries.is_empty() {
            self.status = "Nothing to save".into();
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
        let res = (|| -> Result<(), std::io::Error> {
            use std::io::{BufWriter, Write};
            let f = std::fs::File::create(&dest)?;
            let mut w = BufWriter::new(f);
            for &ei in &m.filtered {
                let e = &m.entries[ei as usize];
                writeln!(
                    w,
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    e.line_no, e.date(), e.time(), e.level.as_char(),
                    e.pid(), e.tid(), e.tag(), e.message(),
                )?;
            }
            w.flush()?;
            Ok(())
        })();
        match res {
            Ok(()) => self.status = format!("Saved {} lines → {}", m.filtered.len(), dest.display()),
            Err(e) => self.status = format!("Save failed: {}", e),
        }
    }

    /// Alt+left-click on a Tag cell → "only this tag".
    fn add_show_tag(&mut self, tag: &str) {
        if tag.is_empty() { return; }
        let mut set = std::collections::HashSet::new();
        set.insert(tag.to_string());
        self.ui.allowed_tags = Some(set);
        self.notify_filter();
    }

    /// Alt+right-click on a Tag cell → exclude this tag.
    fn add_remove_tag(&mut self, tag: &str) {
        if tag.is_empty() { return; }
        let mut set = match self.ui.allowed_tags.clone() {
            Some(s) => s,
            None => self.model.read().unwrap().tag_counts.keys().cloned().collect(),
        };
        set.remove(tag);
        self.ui.allowed_tags = Some(set);
        self.notify_filter();
    }

    fn refresh_devices(&mut self) {
        match adb::list_devices(self.cfg.adb.adb_path.as_deref()) {
            Ok(list) => {
                let n = list.len();
                self.adb_devices = list;
                // Preserve current selection if still present; otherwise pick
                // first device but never overwrite an explicit user choice
                // unless it disappeared.
                if !self.selected_device.is_empty()
                    && !self.adb_devices.iter().any(|d| d == &self.selected_device)
                {
                    self.selected_device = String::new();
                }
                self.status = if n == 0 {
                    tr!("status_adb_devices_zero")
                } else {
                    tr!("status_adb_devices", { n: n })
                };
            }
            Err(e) => {
                self.adb_devices.clear();
                self.status = tr!("status_adb_devices_failed", { e: &format!("{}", e) });
            }
        }
    }

    fn spawn_filter_thread(&self, ctx: egui::Context) {
        let model = self.model.clone();
        let spec_lock = self.shared_filter.clone();
        let gen = self.gen.clone();
        let wake = self.wake.clone();
        thread::Builder::new().name("filter".into()).spawn(move || {
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
                let mut pending = lock.lock().unwrap();
                while !*pending {
                    let (p, _) = cvar.wait_timeout(pending, Duration::from_secs(60)).unwrap();
                    pending = p;
                }
                *pending = false;
                drop(pending);

                let spec_gen = gen.load(Ordering::Acquire);
                let spec = spec_lock.read().unwrap().clone();
                let entries_len = model.read().unwrap().entries.len();

                let full = spec_gen != last_spec_gen || entries_len < processed_len;
                let start = if full { 0 } else { processed_len };

                let cap = if full { entries_len / 4 } else { (entries_len - start) / 2 + 1 };
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
                    let m = model.read().unwrap();
                    let hi = end.min(m.entries.len());
                    for j in i..hi {
                        if spec.matches(&m.entries[j], &m.bookmarks) {
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
                let mut m = model.write().unwrap();
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
        }).expect("spawn filter thread");
    }

    fn toggle_bookmark(&mut self, entry_idx: u32) {
        let mut m = self.model.write().unwrap();
        if m.bookmarks.contains(&entry_idx) {
            m.bookmarks.remove(&entry_idx);
        } else {
            m.bookmarks.insert(entry_idx);
        }
    }

    fn toggle_selected_bookmark(&mut self) {
        let entries: Vec<u32> = {
            let m = self.model.read().unwrap();
            self.selected_rows.iter().filter_map(|&r| m.filtered.get(r).copied()).collect()
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
        }
    }

    fn page_selected_row(&mut self, forward: bool) {
        let len = self.model.read().unwrap().filtered.len();
        let anchor = self.selected_rows.iter().next().copied();
        let Some(row) = page_row(anchor, len, self.visible_table_rows, forward) else {
            return;
        };
        self.select_filtered_row_with_len(row, len);
    }

    /// Move selection by `delta` rows (±1) and update the selection anchor.
    fn move_selected_row(&mut self, delta: isize) {
        let m = self.model.read().unwrap();
        let len = m.filtered.len();
        if len == 0 {
            return;
        }
        let cur = self.selected_rows.iter().next().copied().unwrap_or(0);
        drop(m);
        let new = if delta < 0 {
            cur.saturating_sub(1)
        } else {
            (cur + 1).min(len.saturating_sub(1))
        };
        self.selected_rows.clear();
        self.selected_rows.insert(new);
        self.pending_scroll = Some(new);
        self.selection_anchor = Some(new);
    }

    /// Extend the selection range from `selection_anchor` by `delta` rows (±1).
    fn extend_selection(&mut self, delta: isize) {
        let m = self.model.read().unwrap();
        let len = m.filtered.len();
        if len == 0 {
            return;
        }
        let anchor = self.selection_anchor.unwrap_or(0);
        let cur = self.selected_rows.iter().next().copied().unwrap_or(anchor);
        drop(m);
        let new = if delta < 0 {
            cur.saturating_sub(1)
        } else {
            (cur + 1).min(len.saturating_sub(1))
        };
        let (lo, hi) = if new < anchor { (new, anchor) } else { (anchor, new) };
        self.selected_rows.clear();
        for i in lo..=hi {
            self.selected_rows.insert(i);
        }
        self.pending_scroll = Some(new);
    }

    fn adjust_table_font_size(&mut self, delta: f32) {
        self.cfg.view.font_size = adjusted_table_font_size(self.cfg.view.font_size, delta);
    }

    fn reset_table_font_size(&mut self) {
        self.cfg.view.font_size = Config::default().view.font_size;
    }

    /// Recompute caches for highlight palette & tokens only when source data changed.
    fn refresh_highlight_caches(&mut self) {
        // Palette: rare change (user edits config), but a simple pointer eq is cheap
        // enough to guard the parse loop.
        let palette_raw = &self.cfg.colors.highlights;
        let palette_changed = self.cached_highlight_palette.len() != palette_raw.len();
        if palette_changed {
            self.cached_highlight_palette = palette_raw.iter().map(|s| parse_color(s)).collect();
        }

        // Token caches: only invalidate when the raw filter text changes.
        let hl_raw = if self.ui.highlight_on { self.ui.highlight.as_str() } else { "" };
        let f_raw = if self.ui.find_on { self.ui.find.as_str() } else { "" };
        if self.cached_highlight_raw.as_str() != hl_raw {
            self.cached_highlight_tokens = if hl_raw.is_empty() { vec![] } else { FilterSpec::tokens(hl_raw) };
            self.cached_highlight_raw = hl_raw.to_string();
        }
        if self.cached_find_raw.as_str() != f_raw {
            self.cached_find_tokens = if f_raw.is_empty() { vec![] } else { FilterSpec::tokens(f_raw) };
            self.cached_find_raw = f_raw.to_string();
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};

        let cmd = Modifiers::COMMAND;
        let shortcut = |key| KeyboardShortcut::new(cmd, key);

        // Allow copy even when a text field is focused: text selection is
        // disabled (selectable_labels = false), so there is no conflict.
        if ctx.input_mut(|i| i.consume_shortcut(&shortcut(Key::C))) {
            self.copy_selected_row();
            return;
        }

        if ctx.egui_wants_keyboard_input() {
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
        {
            let shift = ctx.input(|i| i.modifiers.shift);
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                if shift {
                    self.extend_selection(-1);
                } else {
                    self.move_selected_row(-1);
                }
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                if shift {
                    self.extend_selection(1);
                } else {
                    self.move_selected_row(1);
                }
                return;
            }
        }
    }

    fn jump_bookmark(&mut self, forward: bool) {
        let m = self.model.read().unwrap();
        if m.filtered.is_empty() { return; }
        let cur = self.selected_rows.iter().next().copied().unwrap_or(0);
        let indices: Vec<usize> = (0..m.filtered.len())
            .filter(|&i| m.bookmarks.contains(&m.filtered[i]))
            .collect();
        if indices.is_empty() { return; }
        let next = if forward {
            indices.iter().find(|&&i| i > cur).copied().or_else(|| indices.first().copied())
        } else {
            indices.iter().rev().find(|&&i| i < cur).copied().or_else(|| indices.last().copied())
        };
        if let Some(n) = next {
            self.selected_rows.clear();
            self.selected_rows.insert(n);
            self.pending_scroll = Some(n);
            self.selection_anchor = Some(n);
        }
    }

    fn render_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = self.ui.picker.clone() else { return; };

        // Build option list from Model counts (all loaded entries, not filtered).
        let (title, options, current_selected) = {
            let m = self.model.read().unwrap();
            match picker.col {
                PickerCol::Pid => {
                    let mut v: Vec<(String, usize)> = m.pid_counts.iter().map(|(k, &v)| (k.clone(), v)).collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    let sel = self.ui.allowed_pids.clone().unwrap_or_else(|| v.iter().map(|(k, _)| k.clone()).collect());
                    (tr!("filter_pid"), v, sel)
                }
                PickerCol::Tid => {
                    let mut v: Vec<(String, usize)> = m.tid_counts.iter().map(|(k, &v)| (k.clone(), v)).collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    let sel = self.ui.allowed_tids.clone().unwrap_or_else(|| v.iter().map(|(k, _)| k.clone()).collect());
                    (tr!("filter_thread"), v, sel)
                }
                PickerCol::Tag => {
                    let mut v: Vec<(String, usize)> = m.tag_counts.iter().map(|(k, &v)| (k.clone(), v)).collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    let sel = self.ui.allowed_tags.clone().unwrap_or_else(|| v.iter().map(|(k, _)| k.clone()).collect());
                    (tr!("filter_tag"), v, sel)
                }
                PickerCol::Level => {
                    let masks = crate::model::LEVEL_MASKS;
                    let labels = ['V','D','I','W','E','F'];
                    let mut v: Vec<(String, usize)> = Vec::new();
                    for (i, &lv) in masks.iter().enumerate() {
                        let c = m.level_counts[i];
                        if c > 0 { v.push((labels[i].to_string(), c)); }
                        let _ = lv;
                    }
                    // For level, "selected" is a set of char labels
                    let current_mask = self.ui.allowed_levels.unwrap_or(LevelMask::ALL);
                    let sel: std::collections::HashSet<String> = (0..6)
                        .filter(|&i| current_mask.contains(masks[i]))
                        .map(|i| labels[i].to_string())
                        .collect();
                    (tr!("filter_lv"), v, sel)
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
                        ui.add(egui::TextEdit::singleline(&mut search)
                            .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                            .desired_width(f32::INFINITY));
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
                        if ui.small_button(tr!("reset")).on_hover_text(tr!("reset_hover")).clicked() {
                            match picker.col {
                                PickerCol::Pid   => self.ui.allowed_pids = None,
                                PickerCol::Tid   => self.ui.allowed_tids = None,
                                PickerCol::Tag   => self.ui.allowed_tags = None,
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
                                if !search_lower.is_empty() && !val.to_lowercase().contains(&search_lower) {
                                    continue;
                                }
                                let mut on = selected.contains(val);
                                let label = format!("{val}    ({cnt})");
                                if ui.checkbox(&mut on, label).changed() {
                                    if on { selected.insert(val.clone()); } else { selected.remove(val); }
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
            && !area_resp.response.rect.contains(ctx.input(|i| i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO)));
        if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }

        // Apply changes to filter state.
        if changed {
            match picker.col {
                PickerCol::Pid => self.ui.allowed_pids = Some(selected.clone()),
                PickerCol::Tid => self.ui.allowed_tids = Some(selected.clone()),
                PickerCol::Tag => self.ui.allowed_tags = Some(selected.clone()),
                PickerCol::Level => {
                    let masks = crate::model::LEVEL_MASKS;
                    let labels = ['V','D','I','W','E','F'];
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
            let loc = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
            if loc.starts_with("zh") { "zh-CN" } else { "en-US" }
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
        ui.painter().layout_no_wrap(t.to_string(), font.clone(), egui::Color32::WHITE).size().x
    };
    if width(s) <= max_width { return s.to_string(); }
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
        if width(&cand) <= max_width { best = cand; lo = mid + 1; }
        else if mid == 0 { break; }
        else { hi = mid - 1; }
    }
    best
}

fn level_color(lv: LevelMask, cfg: &Config) -> Color32 {
    let s = if lv.contains(LevelMask::F) { &cfg.colors.level_f }
        else if lv.contains(LevelMask::E) { &cfg.colors.level_e }
        else if lv.contains(LevelMask::W) { &cfg.colors.level_w }
        else if lv.contains(LevelMask::I) { &cfg.colors.level_i }
        else if lv.contains(LevelMask::D) { &cfg.colors.level_d }
        else { &cfg.colors.level_v };
    parse_color(s)
}

const EMPTY_SHORTCUT_TOP_PADDING_ROWS: usize = 3;

struct EmptyShortcutRow {
    tag: String,
    message: String,
}

fn empty_shortcut_rows() -> Vec<EmptyShortcutRow> {
    let rows = [
        (tr!("shortcut_file"), format!("Ctrl/Cmd+S - {}", tr!("sh_save"))),
        (tr!("shortcut_bookmarks"), format!("Ctrl/Cmd+F2 - {}", tr!("sh_toggle_bookmark"))),
        (tr!("shortcut_bookmarks"), format!("F2 - {}", tr!("sh_prev_bookmark"))),
        (tr!("shortcut_bookmarks"), format!("F3 - {}", tr!("sh_next_bookmark"))),
        (tr!("shortcut_line"), format!("Ctrl/Cmd+C - {}", tr!("sh_copy_selected"))),
        (tr!("shortcut_line"), format!("PageUp / PageDown - {}", tr!("sh_page_up_down"))),
        (tr!("shortcut_font"), format!("Ctrl/Cmd+Plus / Ctrl/Cmd+Minus - {}", tr!("sh_font_size"))),
        (tr!("shortcut_font"), format!("Ctrl/Cmd+0 - {}", tr!("sh_reset_font"))),
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

fn page_row(selected: Option<usize>, len: usize, visible_rows: usize, forward: bool) -> Option<usize> {
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
    if text.is_empty() { return job; }
    // Fast path: no tokens → plain text, skip to_lowercase allocation.
    if highlights.is_empty() && finds.is_empty() {
        job.append(text, 0.0, TextFormat { color: fg, font_id: font, ..Default::default() });
        return job;
    }
    let low = text.to_lowercase();

    // Collect matches: (start, end, kind) where kind=Some(hi_index) = highlight, None = find-underline
    let mut hits: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for (ti, tok) in highlights.iter().enumerate() {
        if tok.is_empty() { continue; }
        let mut off = 0;
        while let Some(pos) = low[off..].find(tok.as_str()) {
            let s = off + pos;
            hits.push((s, s + tok.len(), Some(ti)));
            off = s + tok.len().max(1);
        }
    }
    for tok in finds {
        if tok.is_empty() { continue; }
        let mut off = 0;
        while let Some(pos) = low[off..].find(tok.as_str()) {
            let s = off + pos;
            hits.push((s, s + tok.len(), None));
            off = s + tok.len().max(1);
        }
    }
    hits.sort_by_key(|h| (h.0, h.1));

    // Merge overlaps: keep earliest-start, longest span; later hits inside get dropped.
    let mut merged: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for h in hits {
        if let Some(last) = merged.last_mut() {
            if h.0 < last.1 {
                if h.1 > last.1 { last.1 = h.1; }
                continue;
            }
        }
        merged.push(h);
    }

    let base = TextFormat { color: fg, font_id: font.clone(), ..Default::default() };
    let mut cursor = 0;
    for (s, e, kind) in merged {
        if s > cursor {
            job.append(&text[cursor..s], 0.0, base.clone());
        }
        let mut fmt = base.clone();
        match kind {
            Some(hi) if !highlight_palette.is_empty() => {
                fmt.background = highlight_palette[hi % highlight_palette.len()];
                fmt.color = Color32::BLACK;
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
        let _ = config::save(&self.cfg);
    }
}

impl App {
    fn ui_menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Menu bar — File · Format · View · Encoding
        let mut recent_open: Option<PathBuf> = None;
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
                    ui.set_min_width(220.0);

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
                                            install_ui_font(&ctx, &self.cfg.view.font, &self.user_font_stems);
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
                        ui.set_min_width(180.0);
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
                        ui.checkbox(&mut self.ui.col_bookmark, tr!("col_mark")).on_hover_text(tr!("col_mark_hover"));
                        ui.checkbox(&mut self.ui.col_line, tr!("col_line"));
                        ui.checkbox(&mut self.ui.col_date, tr!("col_date"));
                        ui.checkbox(&mut self.ui.col_time, tr!("col_time"));
                        ui.checkbox(&mut self.ui.col_loglv, tr!("col_lv"));
                        ui.checkbox(&mut self.ui.col_pid, tr!("col_pid"));
                        ui.checkbox(&mut self.ui.col_thread, tr!("col_thread"));
                        ui.checkbox(&mut self.ui.col_tag, tr!("col_tag"));
                        ui.checkbox(&mut self.ui.col_message, tr!("col_msg"));
                        ui.separator();
                        if ui.button(tr!("show_all")).clicked() {
                            self.ui.col_bookmark = true;
                            self.ui.col_line = true;
                            self.ui.col_date = true;
                            self.ui.col_time = true;
                            self.ui.col_loglv = true;
                            self.ui.col_pid = true;
                            self.ui.col_thread = true;
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

                });

                ui.menu_button(tr!("m_encoding"), |ui| {
                    for (label, value) in [
                        (tr!("local"), "local"),
                        ("UTF-8".to_string(), "utf-8"),
                    ] {
                        let selected = self.ui.encoding == value;
                        if ui.selectable_label(selected, label).clicked() {
                            self.ui.encoding = value.into();
                            ui.close();
                        }
                    }
                });

                ui.menu_button(tr!("m_help"), |ui| {
                    if ui.link(format!("LogFilter v{}", env!("CARGO_PKG_VERSION"))).clicked() {
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
                let r = ui.add(egui::TextEdit::singleline(&mut self.ui.find)
                    .id(egui::Id::new("filter_find_edit"))
                    .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                    .desired_width(w));
                dirty |= r.changed();
            });

            // Row 2: Remove | Highlight
            ui.horizontal(|ui| {
                let avail = ui.available_width();
                let text_w = (avail / 2.0 - 100.0).max(120.0);
                dirty |= ui.checkbox(&mut self.ui.remove_on, tr!("remove")).changed();
                dirty |= ui.add(egui::TextEdit::singleline(&mut self.ui.remove)
                    .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                    .desired_width(text_w)).changed();
                ui.separator();
                dirty |= ui.checkbox(&mut self.ui.highlight_on, tr!("highlight")).changed();
                dirty |= ui.add(egui::TextEdit::singleline(&mut self.ui.highlight)
                    .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                    .desired_width(text_w)).changed();
            });

            // Row 3: adb toolbar + Goto + Auto-scroll
            ui.horizontal_wrapped(|ui| {
                let running = self.adb_session.is_some();
                let cmds = self.cfg.adb.commands.clone();
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
                let devices = self.adb_devices.clone();
                egui::ComboBox::from_id_salt("device")
                    .selected_text(if self.selected_device.is_empty() { tr!("device_any") } else { self.selected_device.clone() })
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.selected_device, String::new(), tr!("device_any"));
                        for d in &devices {
                            ui.selectable_value(&mut self.selected_device, d.clone(), d);
                        }
                    });
                if ui.button("↻").on_hover_text(tr!("refresh_devices")).clicked() {
                    self.refresh_devices();
                }
                ui.separator();
                if ui.button(if running { tr!("restart") } else { tr!("run") }).clicked() {
                    self.adb_run();
                }
                let pause_label = self.adb_session.as_ref().map(|s| if s.is_paused() { tr!("resume") } else { tr!("pause") }).unwrap_or(tr!("pause"));
                if ui.add_enabled(running, egui::Button::new(pause_label)).clicked() {
                    self.adb_pause_toggle();
                }
                if ui.add_enabled(running, egui::Button::new(tr!("stop"))).clicked() {
                    self.adb_stop();
                }
                if ui.button(tr!("clear")).clicked() {
                    self.clear();
                }
                ui.separator();
                ui.label(tr!("goto"));
                let goto_resp = ui.add(egui::TextEdit::singleline(&mut self.ui.goto_line).desired_width(70.0));
                if goto_resp.has_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(n) = self.ui.goto_line.trim().parse::<usize>() {
                        if n > 0 { goto_target = Some(n - 1); }
                    }
                }
                ui.checkbox(&mut self.auto_scroll, tr!("auto_scroll"));
            });
        });
        if dirty { self.notify_filter(); }
        if let Some(row) = goto_target {
            let m = self.model.read().unwrap();
            if let Some(pos) = m.filtered.iter().position(|&e| e as usize == row) {
                self.pending_scroll = Some(pos);
                self.selected_rows.clear();
                self.selected_rows.insert(pos);
            }
        }
    }

    fn ui_status_bar(&mut self, ui: &mut egui::Ui) {
        // Status bar
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            let model = self.model.read().unwrap();
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
                ui.label(format!("{} {}", tr!("filtered"), model.filtered.len()));
                ui.separator();
                ui.label(format!("{} {}", tr!("bookmarks"), model.bookmarks.len()));
                ui.separator();
                ui.label(self.ui.encoding.to_uppercase());
                let n = self.selected_rows.len();
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(&self.status);
                } else if n > 0 {
                    ui.separator();
                    ui.label(format!("Selected {n}"));
                }
            });
        });
    }

    fn ui_indicator(&mut self, ui: &mut egui::Ui) {
        // Indicator panel (mini-scrollbar)
        egui::Panel::right("indicator").exact_size(24.0).resizable(false).show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, Color32::from_gray(30));
            let model = self.model.read().unwrap();
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

            let (cl, cd, ct, clv, cpi, cth, cta, cmk, cms) = (
                tr!("col_line"), tr!("col_date"), tr!("col_time"), tr!("col_lv"),
                tr!("col_pid"), tr!("col_thread"), tr!("col_tag"), tr!("col_mark"),
                tr!("col_message"),
            );
            let cols_show: [(bool, &str, f32); 9] = [
                (self.ui.col_line,     &cl,    60.0),
                (self.ui.col_date,     &cd,    60.0),
                (self.ui.col_time,     &ct,   100.0),
                (self.ui.col_loglv,    &clv,   44.0),
                (self.ui.col_pid,      &cpi,   60.0),
                (self.ui.col_thread,   &cth,   60.0),
                (self.ui.col_tag,      &cta,  140.0),
                (self.ui.col_bookmark, &cmk,   30.0),
                (self.ui.col_message,  &cms,  300.0),
            ];
            let last_visible = cols_show.iter().rposition(|(v, _, _)| *v);
            let table_available_height = ui.available_height();
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
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
                if !*visible { continue; }
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

            let model = self.model.read().unwrap();
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

            // Column meta for header interactions
            #[derive(Clone, Copy)]
            enum ColKind { Line, Date, Time, Lv, Pid, Thread, Tag, Bookmark, Message }
            let col_kinds: [ColKind; 9] = [
                ColKind::Line, ColKind::Date, ColKind::Time, ColKind::Lv,
                ColKind::Pid, ColKind::Thread, ColKind::Tag, ColKind::Bookmark, ColKind::Message,
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
                        if !*visible { continue; }
                        let kind = col_kinds[i];
                        let pk = picker_of(kind);
                        // Dropdown marker: use ⏷ (U+23F7), which lives in
                        // emoji-icon-font — part of the Monospace fallback chain.
                        // ▾/▼ (U+25BE/25BC) exist only in Hack, which the
                        // "table font follows the menu" mirror dropped from
                        // Monospace, so they'd render as tofu boxes.
                        let label = if pk.is_some() {
                            format!("{name} \u{23F7}")
                        } else {
                            name.to_string()
                        };
                        h.col(|ui| {
                            let resp = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&label)
                                        .font(font.clone())
                                        .strong(),
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
                                if ui.add_enabled(can_filter, egui::Button::new(tr!("filter_this"))).clicked() {
                                    if let Some(p) = pk {
                                        open_picker = Some((p, resp.rect.left_bottom()));
                                    }
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button(tr!("hide_this")).clicked() {
                                    hide_col_idx = Some(i);
                                    ui.close();
                                }
                            });
                        });
                    }
                })
                .body(|body| {
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
                            let shortcut = &shortcut_rows[row.index() - EMPTY_SHORTCUT_TOP_PADDING_ROWS];
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
                        let col = level_color(e.level, &self.cfg);
                        let is_selected = sel.contains(&row_idx);
                        let is_bookmarked = bookmarks.contains(&entry_idx);
                        row.set_selected(is_selected);

                        // Render each cell with the configured monospace font so
                        // View → Font size affects *every* column, not just Tag/Message.
                        // truncate() keeps every cell on a single line (… on overflow).
                        let render = |ui: &mut egui::Ui, s: &str| {
                            ui.add(egui::Label::new(
                                egui::RichText::new(s).font(font.clone()).color(col),
                            ).truncate());
                        };

                        if self.ui.col_line     { row.col(|ui| { render(ui, &e.line_no.to_string()); }); }
                        if self.ui.col_date     { row.col(|ui| { render(ui, e.date()); }); }
                        if self.ui.col_time     { row.col(|ui| { render(ui, e.time()); }); }
                        if self.ui.col_loglv    { row.col(|ui| { render(ui, &e.level.as_char().to_string()); }); }
                        if self.ui.col_pid      { row.col(|ui| { render(ui, e.pid()); }); }
                        if self.ui.col_thread   { row.col(|ui| { render(ui, e.tid()); }); }
                        if self.ui.col_tag {
                            // Render a plain (non-interactive) label so the *cell*
                            // keeps the pointer hover — an inner Sense::click label
                            // would steal it and kill the row-hover highlight.
                            let (_, resp) = row.col(|ui| {
                                if use_highlight {
                                    let job = build_highlighted(
                                        e.tag(), highlight_tokens, find_tokens,
                                        col, font.clone(), highlight_palette,
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
                                        e.message(), highlight_tokens, find_tokens,
                                        col, font.clone(), highlight_palette,
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
                                            entries, filtered, &self.selected_rows, |e| e.message()
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
                                        copy_cell_text = Some(format!(
                                            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                            e.line_no, e.date(), e.time(), e.level.as_char(),
                                            e.pid(), e.tid(), e.tag(), e.message()
                                        ));
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
                    self.status = format!("Ctrl+click, selected={}", self.selected_rows.len());
                } else if shift {
                    let anchor = self.selected_rows.iter().next().copied().unwrap_or(0);
                    let (lo, hi) = if r < anchor { (r, anchor) } else { (anchor, r) };
                    for i in lo..=hi { self.selected_rows.insert(i); }
                    self.status = format!("Shift+click, selected={}", self.selected_rows.len());
                } else {
                    self.selected_rows.clear();
                    self.selected_rows.insert(r);
                    self.selection_anchor = Some(r);
                }
            }
            if let Some(r) = double_clicked_row {
                let entry_idx = self.model.read().unwrap().filtered.get(r).copied();
                if let Some(i) = entry_idx { self.toggle_bookmark(i); }
                self.selected_rows.clear();
                self.selected_rows.insert(r);
                self.selection_anchor = Some(r);
            }
            if let Some(t) = alt_left_tag { self.add_show_tag(&t); }
            if let Some(t) = alt_right_tag { self.add_remove_tag(&t); }
            if let Some(txt) = copy_cell_text {
                let n = txt.lines().count();
                match arboard::Clipboard::new() {
                    Ok(mut c) => {
                        let _ = c.set_text(&txt);
                        self.status = if n > 1 { format!("Copied {n} rows") } else { "Copied 1 row".into() };
                    }
                    Err(e) => self.status = format!("Clipboard error: {e}"),
                }
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
                    6 => self.ui.col_tag = false,
                    7 => self.ui.col_bookmark = false,
                    8 => self.ui.col_message = false,
                    _ => {}
                }
            }
            // Open picker requested from column-header click or context menu.
            if let Some((col, anchor)) = open_picker {
                self.ui.picker = Some(PickerState { col, search: String::new(), anchor, just_opened: true });
            }
        });
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LogEntry;
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
                &[b'l', b'a', b's', b't'][..],
            ]
            .concat(),
        )
        .unwrap();

        let (tx, rx) = bounded(8);
        let source_epoch = Arc::new(AtomicU64::new(42));
        send_utf8_lines(&path, tx, 42, source_epoch);
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
    fn copy_selected_rows_text_joins_multiple_rows() {
        use crate::model::LevelMask;
        let model = Model {
            entries: vec![
                LogEntry::from_fields("07-10", "10:00:00.000", LevelMask::I, "100", "200", "Tag1", "msg one"),
                LogEntry::from_fields("07-10", "10:00:01.000", LevelMask::E, "101", "201", "Tag2", "msg two"),
                LogEntry::from_fields("07-10", "10:00:02.000", LevelMask::W, "102", "202", "Tag3", "msg three"),
            ],
            filtered: vec![0, 1, 2],
            ..Model::default()
        };
        let model = Arc::new(RwLock::new(model));

        let mut selected_rows = HashSet::new();
        selected_rows.insert(0);
        selected_rows.insert(2);

        // Unit-test the core copy logic without building a full App.
        let text = {
            let m = model.read().unwrap();
            let mut rows: Vec<&usize> = selected_rows.iter().collect();
            rows.sort();
            let texts: Vec<String> = rows.iter().filter_map(|&&r| {
                let &ei = m.filtered.get(r)?;
                let e = &m.entries[ei as usize];
                Some(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    e.line_no, e.date(), e.time(), e.level.as_char(), e.pid(), e.tid(), e.tag(), e.message()
                ))
            }).collect();
            texts.join("\n")
        };
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 2, "should have 2 lines (rows 0 and 2), got: {lines:?}");
        assert!(lines[0].contains("msg one"), "first line should contain msg one: {lines:?}");
        assert!(lines[1].contains("msg three"), "second line should contain msg three: {lines:?}");
    }
}
