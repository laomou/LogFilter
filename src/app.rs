use crate::adb;
use crate::config::{self, parse_color, Config};
use crate::filter::FilterSpec;
use crate::model::{EncodingChoice, LevelMask, Model};
use crate::parser::parse_line;
use egui_i18n::tr;
use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};
use egui_extras::{Column, TableBuilder};
use encoding_rs::Encoding;
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
    pub wake: Arc<(Mutex<bool>, Condvar)>,
    pub status: String,
    pub ui: UiState,

    pub selected_row: Option<usize>,
    pub pending_scroll: Option<usize>,
    pub focus_find: bool,
    pub last_filtered_len: usize,

    // Font stems currently registered as FontFamily::Name(..); only these are
    // safe to use for per-font previews (unregistered names panic in egui).
    pub registered_fonts: Vec<String>,

    // adb
    pub line_tx: Sender<String>,
    pub adb_session: Option<adb::Session>,
    pub adb_devices: Vec<String>,
    pub selected_device: String,
    pub selected_cmd: String,
    pub auto_scroll: bool,
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
        disable_hover_highlight(&cc.egui_ctx);
        let cfg = config::load();
        init_i18n();
        // Apply the stored language (or auto-detect) at startup.
        resolve_startup_lang(&cfg.view.lang);
        let registered_fonts = install_ui_font(&cc.egui_ctx, &cfg.view.font);
        bump_global_text_sizes(&cc.egui_ctx);
        // egui defaults Ctrl+= / Ctrl+- / Ctrl+0 to changing the global zoom_factor,
        // which scales the entire UI (menus, toolbar, table). We only want those
        // shortcuts to change the table font size, so disable egui's handler and
        // implement our own in `update()`.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let ui = UiState::from_config(&cfg);
        let shared_filter = Arc::new(RwLock::new(ui.to_filter_spec()));
        let (line_tx, line_rx) = unbounded::<String>();
        let selected_cmd = cfg.adb.commands.first().cloned().unwrap_or_else(|| "logcat -v threadtime".into());
        let mut app = Self {
            cfg,
            model: Arc::new(RwLock::new(Model::default())),
            shared_filter,
            gen: Arc::new(AtomicU64::new(0)),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            status: String::new(),
            ui,
            selected_row: None,
            pending_scroll: None,
            focus_find: false,
            last_filtered_len: 0,
            registered_fonts,
            line_tx,
            adb_session: None,
            adb_devices: Vec::new(),
            selected_device: String::new(),
            selected_cmd,
            auto_scroll: true,
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
        let bytes = std::fs::read(path)?;
        let text = decode_bytes(&bytes, self.encoding_choice());
        {
            let mut model = self.model.write().unwrap();
            model.clear();
            model.file_path = Some(path.to_path_buf());
            for line in text.lines() {
                let (entry, _fmt) = parse_line(line);
                model.append(entry);
            }
            model.filtered = (0..model.entries.len() as u32).collect();
            self.status = tr!("status_loaded", { path: &path.display().to_string(), n: model.entries.len() });
        }
        self.selected_row = None;
        config::add_recent(&mut self.cfg, path);
        self.notify_filter();
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
    }

    fn spawn_ingest_thread(&self, ctx: egui::Context, rx: Receiver<String>) {
        let model = self.model.clone();
        let gen = self.gen.clone();
        let wake = self.wake.clone();
        thread::Builder::new().name("ingest".into()).spawn(move || {
            let mut batch: Vec<String> = Vec::with_capacity(256);
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
                {
                    let mut m = model.write().unwrap();
                    for line in batch.drain(..) {
                        let (entry, _) = parse_line(&line);
                        m.append(entry);
                    }
                }
                gen.fetch_add(1, Ordering::AcqRel);
                let (lock, cvar) = &*wake;
                *lock.lock().unwrap() = true;
                cvar.notify_one();
                ctx.request_repaint();
            }
        }).expect("spawn ingest thread");
    }

    fn adb_run(&mut self) {
        self.adb_stop();
        let device = if self.selected_device.is_empty() { None } else { Some(self.selected_device.as_str()) };
        match adb::Session::start(
            self.cfg.adb.adb_path.as_deref(),
            device,
            &self.selected_cmd,
            self.line_tx.clone(),
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
        self.selected_row = None;
        self.notify_filter();
    }

    #[allow(dead_code)]
    fn copy_selected_row(&self) {
        let Some(r) = self.selected_row else { return; };
        let m = self.model.read().unwrap();
        let Some(&ei) = m.filtered.get(r) else { return; };
        let e = &m.entries[ei as usize];
        let text = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            e.line_no, e.date, e.time, e.level.as_char(), e.pid, e.tid, e.tag, e.message
        );
        let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(text));
    }

    fn save_filtered(&mut self) {
        let m = self.model.read().unwrap();
        if m.filtered.is_empty() && m.entries.is_empty() {
            self.status = "Nothing to save".into();
            return;
        }
        let path = rfd::FileDialog::new()
            .set_file_name("logfilter.tsv")
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
                    e.line_no, e.date, e.time, e.level.as_char(),
                    e.pid, e.tid, e.tag, e.message,
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
            loop {
                let mut pending = lock.lock().unwrap();
                while !*pending {
                    let (p, _) = cvar.wait_timeout(pending, Duration::from_secs(60)).unwrap();
                    pending = p;
                }
                *pending = false;
                drop(pending);

                let gen_start = gen.load(Ordering::Acquire);
                let spec = spec_lock.read().unwrap().clone();
                let bookmarks = { model.read().unwrap().bookmarks.clone() };
                let entries_len = model.read().unwrap().entries.len();

                let mut out: Vec<u32> = Vec::with_capacity(entries_len / 4);
                let mut aborted = false;
                for i in 0..entries_len {
                    if i % 4096 == 0 && gen.load(Ordering::Acquire) != gen_start {
                        aborted = true;
                        break;
                    }
                    let matched = {
                        let m = model.read().unwrap();
                        if i >= m.entries.len() { break; }
                        spec.matches(&m.entries[i], &bookmarks)
                    };
                    if matched {
                        out.push(i as u32);
                    }
                }

                if !aborted && gen.load(Ordering::Acquire) == gen_start {
                    let mut m = model.write().unwrap();
                    m.filtered = out;
                    drop(m);
                    ctx.request_repaint();
                }
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

    #[allow(dead_code)]
    fn jump_bookmark(&mut self, forward: bool) {
        let m = self.model.read().unwrap();
        if m.filtered.is_empty() { return; }
        let cur = self.selected_row.unwrap_or(0);
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
            self.selected_row = Some(n);
            self.pending_scroll = Some(n);
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
                        ui.add(egui::TextEdit::singleline(&mut search).desired_width(f32::INFINITY));
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

        // Persist picker search text.
        if let Some(p) = self.ui.picker.as_mut() {
            p.search = search;
        }

        // If user clicked outside the picker, close it.
        let clicked_outside = ctx.input(|i| i.pointer.any_click())
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

fn disable_hover_highlight(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals.widgets;
    // Copy inactive presentation onto hovered so pointer-over produces no
    // visible change; keep `active` (mouse-down) as-is for click feedback.
    v.hovered.bg_fill = v.inactive.bg_fill;
    v.hovered.weak_bg_fill = v.inactive.weak_bg_fill;
    v.hovered.bg_stroke = v.inactive.bg_stroke;
    v.hovered.fg_stroke = v.inactive.fg_stroke;
    v.hovered.expansion = 0.0;
    // Labels should never enter text-selection mode; keeps I-beam cursor off
    // read-only table cells.
    style.interaction.selectable_labels = false;
    ctx.set_style(style);
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

/// Load user-supplied fonts from `~/.config/logfilter/fonts/`. No fonts are
/// embedded and no system paths are probed.
///
/// `primary` is the file stem of the font to hoist to the front of both the
/// Proportional and Monospace family stacks, making it the active face for the
/// table and most UI text. Empty = no primary (egui built-ins stay primary,
/// user fonts are fallbacks in filename order).
///
/// Each loaded font is also exposed under its own `FontFamily::Name(stem)` so
/// the Font menu can render each entry as a preview in that font.
///
/// Returns the list of registered font stems, so callers can track which
/// `FontFamily::Name(..)` values are safe to use for previews (using an
/// unregistered named family panics inside egui).
fn install_ui_font(ctx: &egui::Context, primary: &str) -> Vec<String> {
    let mut fonts = egui::FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();

    if let Some(dir) = config::fonts_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
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
            for p in entries {
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("font")
                    .to_string();
                if let Ok(bytes) = std::fs::read(&p) {
                    fonts
                        .font_data
                        .insert(name.clone(), egui::FontData::from_owned(bytes));
                    // Named family so the picker can preview this font alone.
                    fonts.families.insert(
                        egui::FontFamily::Name(name.clone().into()),
                        vec![name.clone()],
                    );
                    added.push(name);
                }
            }
        }
    }

    // Hoist the selected primary to the front of the user-font list.
    if !primary.is_empty() && added.iter().any(|n| n == primary) {
        added.sort_by_key(|n| n != primary);
    }

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        // Prepend user fonts (selected primary first) so the chosen face
        // actually takes effect; egui's built-in faces stay as last-resort
        // fallback for any glyph the selected font lacks.
        for (i, name) in added.iter().enumerate() {
            list.insert(i, name.clone());
        }
    }
    ctx.set_fonts(fonts);
    added
}

/// egui's stock text sizes (Body 14, Button 14, Small 10) render a bit small on
/// modern high-DPI displays; bump them up ~1pt so menus/toolbars/status match the
/// table density chosen via View → Font size (default 14).
fn bump_global_text_sizes(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    use egui::TextStyle;
    for (style_key, size) in [
        (TextStyle::Body, 15.0),
        (TextStyle::Button, 15.0),
        (TextStyle::Monospace, 14.0),
        (TextStyle::Small, 12.0),
        (TextStyle::Heading, 20.0),
    ] {
        if let Some(id) = style.text_styles.get_mut(&style_key) {
            id.size = size;
        }
    }
    ctx.set_style(style);
}

fn decode_bytes(bytes: &[u8], choice: EncodingChoice) -> String {
    match choice {
        EncodingChoice::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        EncodingChoice::Local => {
            let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
            let enc = pick_local_encoding(&locale);
            let (cow, _, _) = enc.decode(bytes);
            cow.into_owned()
        }
    }
}

fn pick_local_encoding(locale: &str) -> &'static Encoding {
    let low = locale.to_lowercase();
    if low.starts_with("zh") { encoding_rs::GBK }
    else if low.starts_with("ja") { encoding_rs::SHIFT_JIS }
    else if low.starts_with("ko") { encoding_rs::EUC_KR }
    else { encoding_rs::WINDOWS_1252 }
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

/// Build a LayoutJob rendering `text` with highlight tokens as background spans
/// and find tokens as underlined spans. All tokens are matched case-insensitively.
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
            _ => {
                fmt.underline = egui::Stroke::new(1.5, fg);
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui_menu_bar(ctx);
        self.ui_options_panel(ctx);
        self.ui_status_bar(ctx);
        self.ui_indicator(ctx);
        self.ui_table(ctx);
        // Column picker popup (Excel-style)
        self.render_picker(ctx);

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
        let _ = config::save(&self.cfg);
    }
}

impl App {
    fn ui_menu_bar(&mut self, ctx: &egui::Context) {
        // Menu bar — File · Format · View · Encoding
        let mut recent_open: Option<PathBuf> = None;
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button(tr!("m_file"), |ui| {
                    if ui.button(tr!("open")).clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            if let Err(e) = self.open_file(&path) {
                                self.status = tr!("status_failed_open", { e: &format!("{}", e) });
                            }
                        }
                        ui.close_menu();
                    }
                    ui.menu_button(tr!("recent"), |ui| {
                        let recent = self.cfg.recent.files.clone();
                        if recent.is_empty() {
                            ui.label(tr!("recent_empty"));
                        }
                        for p in recent {
                            if ui.button(p.display().to_string()).clicked() {
                                recent_open = Some(p);
                                ui.close_menu();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button(tr!("save_filtered")).clicked() {
                        self.save_filtered();
                        ui.close_menu();
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
                        ui.set_min_width(320.0);
                        if let Some(dir) = config::fonts_dir() {
                            let loaded: Vec<(String, String)> = std::fs::read_dir(&dir)
                                .map(|rd| {
                                    rd.flatten()
                                        .filter_map(|e| {
                                            let p = e.path();
                                            let ext = p.extension()
                                                .and_then(|s| s.to_str())
                                                .map(|s| s.to_ascii_lowercase());
                                            if !matches!(ext.as_deref(), Some("ttf") | Some("otf") | Some("ttc") | Some("otc")) {
                                                return None;
                                            }
                                            let stem = p.file_stem()?.to_str()?.to_string();
                                            let name = p.file_name()?.to_str()?.to_string();
                                            Some((stem, name))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();

                            if loaded.is_empty() {
                                ui.label(egui::RichText::new(dir.display().to_string()).small().weak());
                            } else {
                                egui::ScrollArea::vertical()
                                    .max_height(220.0)
                                    .show(ui, |ui| {
                                        for (stem, name) in &loaded {
                                            let sel = self.cfg.view.font == *stem;
                                            let text = format!("{}  —  AaBb 中文 123", name);
                                            // Only render the preview in the font's own
                                            // face if it's actually registered; an
                                            // unregistered FontFamily::Name panics in egui
                                            // (e.g. a font dropped into the folder after
                                            // startup that hasn't been loaded yet).
                                            let label = if self.registered_fonts.iter().any(|f| f == stem) {
                                                egui::RichText::new(text)
                                                    .family(egui::FontFamily::Name(stem.clone().into()))
                                            } else {
                                                egui::RichText::new(text)
                                            };
                                            let resp = ui.add(egui::SelectableLabel::new(sel, label));
                                            if resp.clicked() && !sel {
                                                self.cfg.view.font = stem.clone();
                                                self.registered_fonts = install_ui_font(ctx, &self.cfg.view.font);
                                                ui.close_menu();
                                            }
                                        }
                                    });
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
                            if ui.add(egui::SelectableLabel::new(sel, format!("{:.0} pt", p))).clicked() {
                                self.cfg.view.font_size = p;
                                ui.close_menu();
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
                            ui.close_menu();
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
                                ui.close_menu();
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
                            ui.close_menu();
                        }
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

    fn ui_options_panel(&mut self, ctx: &egui::Context) {
        // Option panel — 3 rows:
        //   Row 1: 🔍 Find (fills width)
        //   Row 2: Remove (half) · Highlight (half)
        //   Row 3: adb toolbar · Goto · Auto-scroll
        let mut dirty = false;
        let mut goto_target: Option<usize> = None;
        egui::TopBottomPanel::top("options").show(ctx, |ui| {
            // Row 1: Find
            ui.horizontal(|ui| {
                dirty |= ui.checkbox(&mut self.ui.find_on, tr!("find")).changed();
                let w = (ui.available_width() - 8.0).max(200.0);
                let r = ui.add(egui::TextEdit::singleline(&mut self.ui.find)
                    .id(egui::Id::new("filter_find_edit"))
                    .desired_width(w));
                dirty |= r.changed();
                if self.focus_find { r.request_focus(); self.focus_find = false; }
            });

            // Row 2: Remove | Highlight
            ui.horizontal(|ui| {
                let avail = ui.available_width();
                let text_w = (avail / 2.0 - 100.0).max(120.0);
                dirty |= ui.checkbox(&mut self.ui.remove_on, tr!("remove")).changed();
                dirty |= ui.add(egui::TextEdit::singleline(&mut self.ui.remove).desired_width(text_w)).changed();
                ui.separator();
                dirty |= ui.checkbox(&mut self.ui.highlight_on, tr!("highlight")).changed();
                dirty |= ui.add(egui::TextEdit::singleline(&mut self.ui.highlight).desired_width(text_w)).changed();
            });

            // Row 3: adb toolbar + Goto + Auto-scroll
            ui.horizontal_wrapped(|ui| {
                let running = self.adb_session.is_some();
                let cmds = self.cfg.adb.commands.clone();
                ui.label(tr!("cmd"));
                egui::ComboBox::from_id_source("cmd")
                    .selected_text(&self.selected_cmd)
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for c in &cmds {
                            ui.selectable_value(&mut self.selected_cmd, c.clone(), c);
                        }
                    });
                ui.label(tr!("device"));
                let devices = self.adb_devices.clone();
                egui::ComboBox::from_id_source("device")
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
                if goto_resp.changed() {
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
                self.selected_row = Some(pos);
            }
        }
    }

    fn ui_status_bar(&mut self, ctx: &egui::Context) {
        // Status bar
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            let model = self.model.read().unwrap();
            ui.horizontal(|ui| {
                let path = model.file_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| tr!("no_file").into());
                ui.label(&path);
                ui.separator();
                ui.label(format!("{} {}", tr!("total"), model.entries.len()));
                ui.separator();
                ui.label(format!("{} {}", tr!("filtered"), model.filtered.len()));
                ui.separator();
                ui.label(format!("{} {}", tr!("bookmarks"), model.bookmarks.len()));
                ui.separator();
                if let Some(r) = self.selected_row {
                    if let Some(&i) = model.filtered.get(r) {
                        ui.label(format!("{} {}", tr!("line"), i + 1));
                        ui.separator();
                    }
                }
                ui.label(self.ui.encoding.to_uppercase());
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(&self.status);
                }
            });
        });
    }

    fn ui_indicator(&mut self, ctx: &egui::Context) {
        // Indicator panel (mini-scrollbar)
        egui::SidePanel::right("indicator").exact_width(24.0).resizable(false).show(ctx, |ui| {
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
                // Bookmarks (left, blue)
                for (fi, &ei) in model.filtered.iter().enumerate() {
                    if model.bookmarks.contains(&ei) {
                        let y = left_col.min.y + h * (fi as f32) / (total as f32);
                        painter.rect_filled(
                            egui::Rect::from_min_size(egui::pos2(left_col.min.x, y), egui::vec2(left_col.width(), 2.0)),
                            0.0,
                            Color32::from_rgb(80, 140, 255),
                        );
                    }
                }
                // Errors (right, red)
                for (fi, &ei) in model.filtered.iter().enumerate() {
                    let e = &model.entries[ei as usize];
                    if e.level.contains(LevelMask::E) || e.level.contains(LevelMask::F) {
                        let y = right_col.min.y + h * (fi as f32) / (total as f32);
                        painter.rect_filled(
                            egui::Rect::from_min_size(egui::pos2(right_col.min.x, y), egui::vec2(right_col.width(), 2.0)),
                            0.0,
                            Color32::from_rgb(255, 80, 80),
                        );
                    }
                }
                // Handle click to jump
                if let Some(pos) = response.interact_pointer_pos() {
                    let frac = ((pos.y - rect.min.y) / h).clamp(0.0, 1.0);
                    let target = (frac * total as f32) as usize;
                    self.pending_scroll = Some(target.min(total.saturating_sub(1)));
                    self.selected_row = self.pending_scroll;
                }
            }
        });
    }

    fn ui_table(&mut self, ctx: &egui::Context) {
        // Log table
        egui::CentralPanel::default().show(ctx, |ui| {
            let font = FontId::monospace(self.cfg.view.font_size);
            let highlight_palette: Vec<Color32> =
                self.cfg.colors.highlights.iter().map(|s| parse_color(s)).collect();
            let highlight_tokens: Vec<String> = if self.ui.highlight_on {
                FilterSpec::tokens(&self.ui.highlight)
            } else { Vec::new() };
            let find_tokens: Vec<String> = if self.ui.find_on {
                FilterSpec::tokens(&self.ui.find)
            } else { Vec::new() };

            let (cl, cd, ct, clv, cpi, cth, cta, cmk, cms) = (
                tr!("col_line"), tr!("col_date"), tr!("col_time"), tr!("col_lv"),
                tr!("col_pid"), tr!("col_thread"), tr!("col_tag"), tr!("col_mark"),
                tr!("col_message"),
            );
            let cols_show: [(bool, &str, f32); 9] = [
                (self.ui.col_line,     &cl,    60.0),
                (self.ui.col_date,     &cd,    60.0),
                (self.ui.col_time,     &ct,   100.0),
                (self.ui.col_loglv,    &clv,   24.0),
                (self.ui.col_pid,      &cpi,   60.0),
                (self.ui.col_thread,   &cth,   60.0),
                (self.ui.col_tag,      &cta,  140.0),
                (self.ui.col_bookmark, &cmk,   30.0),
                (self.ui.col_message,  &cms,  300.0),
            ];
            let last_visible = cols_show.iter().rposition(|(v, _, _)| *v);

            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                // Fill all available vertical space instead of egui_extras'
                // default 800px cap / content-shrink, so the table uses the whole
                // window when maximized.
                .auto_shrink([false, false])
                .max_scroll_height(f32::INFINITY)
                .sense(egui::Sense::click());
            for (i, (visible, _, w)) in cols_show.iter().enumerate() {
                if !*visible { continue; }
                if Some(i) == last_visible {
                    table = table.column(Column::remainder().at_least(*w));
                } else {
                    table = table.column(Column::initial(*w).at_least(*w * 0.5));
                }
            }
            {
                let cur_len = self.model.read().unwrap().filtered.len();
                if self.auto_scroll && cur_len > self.last_filtered_len && cur_len > 0 {
                    self.pending_scroll.get_or_insert(cur_len - 1);
                }
                self.last_filtered_len = cur_len;
            }
            if let Some(scroll_to) = self.pending_scroll.take() {
                table = table.scroll_to_row(scroll_to, Some(egui::Align::Center));
            }
            let selected = self.selected_row;

            let model = self.model.read().unwrap();
            let entries = &model.entries;
            let filtered = &model.filtered;
            let bookmarks = &model.bookmarks;

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

            table
                .header(header_h, |mut h| {
                    for (i, (visible, name, _)) in cols_show.iter().enumerate() {
                        if !*visible { continue; }
                        let kind = col_kinds[i];
                        let pk = picker_of(kind);
                        let has_filter_active = match pk {
                            Some(PickerCol::Level) => self.ui.allowed_levels.is_some(),
                            Some(PickerCol::Pid) => self.ui.allowed_pids.is_some(),
                            Some(PickerCol::Tid) => self.ui.allowed_tids.is_some(),
                            Some(PickerCol::Tag) => self.ui.allowed_tags.is_some(),
                            None => false,
                        };
                        let label = if pk.is_some() {
                            if has_filter_active { format!("{name} ▼") } else { format!("{name} ▾") }
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
                                    ui.close_menu();
                                }
                                ui.separator();
                                if ui.button(tr!("hide_this")).clicked() {
                                    hide_col_idx = Some(i);
                                    ui.close_menu();
                                }
                                if ui.button(tr!("autosize")).clicked() {
                                    ui.close_menu(); // TableBuilder auto-sizes by default; no-op stub
                                }
                            });
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, filtered.len(), |mut row| {
                        let row_idx = row.index();
                        let entry_idx = filtered[row_idx];
                        let e = &entries[entry_idx as usize];
                        let col = level_color(e.level, &self.cfg);
                        let is_selected = selected == Some(row_idx);
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
                        if self.ui.col_date     { row.col(|ui| { render(ui, &e.date); }); }
                        if self.ui.col_time     { row.col(|ui| { render(ui, &e.time); }); }
                        if self.ui.col_loglv    { row.col(|ui| { render(ui, &e.level.as_char().to_string()); }); }
                        if self.ui.col_pid      { row.col(|ui| { render(ui, &e.pid); }); }
                        if self.ui.col_thread   { row.col(|ui| { render(ui, &e.tid); }); }
                        if self.ui.col_tag {
                            row.col(|ui| {
                                let job = build_highlighted(
                                    &e.tag, &highlight_tokens, &find_tokens,
                                    col, font.clone(), &highlight_palette,
                                );
                                let resp = ui.add(egui::Label::new(job).truncate().sense(egui::Sense::click()));
                                if alt && resp.clicked() {
                                    alt_left_tag = Some(e.tag.clone());
                                } else if resp.clicked() {
                                    // Plain click on the Tag cell selects the row.
                                    clicked_row = Some(row_idx);
                                }
                                if resp.double_clicked() {
                                    double_clicked_row = Some(row_idx);
                                }
                                if alt && resp.secondary_clicked() {
                                    alt_right_tag = Some(e.tag.clone());
                                }
                                resp.context_menu(|ui| {
                                    if ui.button(tr!("copy_tag")).clicked() {
                                        copy_cell_text = Some(e.tag.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button(tr!("add_show_tag")).clicked() {
                                        alt_left_tag = Some(e.tag.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button(tr!("add_remove_tag")).clicked() {
                                        alt_right_tag = Some(e.tag.clone());
                                        ui.close_menu();
                                    }
                                });
                            });
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
                            row.col(|ui| {
                                let job = build_highlighted(
                                    &e.message, &highlight_tokens, &find_tokens,
                                    col, font.clone(), &highlight_palette,
                                );
                                let resp = ui.add(egui::Label::new(job).truncate().sense(egui::Sense::click()));
                                // The label consumes clicks, so propagate a plain
                                // left-click to row selection.
                                if resp.clicked() {
                                    clicked_row = Some(row_idx);
                                }
                                if resp.double_clicked() {
                                    double_clicked_row = Some(row_idx);
                                }
                                resp.context_menu(|ui| {
                                    if ui.button(tr!("copy_message")).clicked() {
                                        copy_cell_text = Some(e.message.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button(tr!("copy_row")).clicked() {
                                        copy_cell_text = Some(format!(
                                            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                            e.line_no, e.date, e.time, e.level.as_char(),
                                            e.pid, e.tid, e.tag, e.message
                                        ));
                                        ui.close_menu();
                                    }
                                });
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

            drop(model);
            if let Some(r) = clicked_row { self.selected_row = Some(r); }
            if let Some(r) = double_clicked_row {
                let entry_idx = self.model.read().unwrap().filtered.get(r).copied();
                if let Some(i) = entry_idx { self.toggle_bookmark(i); }
                self.selected_row = Some(r);
            }
            if let Some(t) = alt_left_tag { self.add_show_tag(&t); }
            if let Some(t) = alt_right_tag { self.add_remove_tag(&t); }
            if let Some(txt) = copy_cell_text {
                let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(txt));
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
                self.ui.picker = Some(PickerState { col, search: String::new(), anchor });
            }
        });
    }

}

