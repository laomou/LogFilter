//! Bounded-memory browsing for very large UTF-8 log files.
//!
//! Page boundaries obey both a row cap and a conservative byte budget. The
//! sparse index stores only each page start; filtered indexes use the same
//! boundaries over matching rows. Page reads for a source are serialized.

use crate::filter::FilterSpec;
use crate::lock::MutexExt;
use crate::model::{LogEntry, LogFormat};
use crate::parser::parse_line_hinted;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub const CHECKPOINT_ROWS: u64 = 20_000;
pub const ESTIMATE_FROM_BYTES: u64 = 256 * 1024 * 1024;
pub const FORCE_PAGING_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PAGE_ROWS: u64 = 5_000_000;
const SAMPLE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct Checkpoint {
    source_line: u64,
    offset: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    identity: crate::io::FileIdentity,
}

impl FileStamp {
    fn read(path: &Path) -> io::Result<Self> {
        Self::from_file(&File::open(path)?)
    }

    fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: crate::io::file_identity(file)?,
        })
    }

    fn validate(self, path: &Path, file: &File) -> io::Result<()> {
        if Self::from_file(file)? == self && Self::read(path)? == self {
            Ok(())
        } else {
            Err(io::Error::other(
                "file changed while the paged snapshot was open; reopen it",
            ))
        }
    }
}

pub struct PagedIndex {
    pub path: PathBuf,
    pub file_len: u64,
    stamp: FileStamp,
    pub indexed_bytes: AtomicU64,
    pub total_rows: AtomicU64,
    pub complete: AtomicBool,
    pub error: Mutex<Option<String>>,
    page_rows: u64,
    page_budget: u64,
    // A replaced request must release its page before another starts allocating.
    page_gate: Arc<Mutex<()>>,
    filter_gate: Arc<Mutex<()>>,
    checkpoints: Mutex<Vec<Checkpoint>>,
    cancel: AtomicBool,
}

impl PagedIndex {
    fn new(path: PathBuf, stamp: FileStamp, page_rows: u64, page_budget: u64) -> Arc<Self> {
        Arc::new(Self {
            path,
            file_len: stamp.len,
            stamp,
            indexed_bytes: AtomicU64::new(0),
            total_rows: AtomicU64::new(0),
            complete: AtomicBool::new(false),
            error: Mutex::new(None),
            page_rows,
            page_budget,
            page_gate: Arc::new(Mutex::new(())),
            filter_gate: Arc::new(Mutex::new(())),
            checkpoints: Mutex::new(vec![Checkpoint {
                source_line: 0,
                offset: 0,
            }]),
            cancel: AtomicBool::new(false),
        })
    }

    pub fn pages(&self) -> u64 {
        self.checkpoints.lock_recover().len() as u64
    }

    pub(crate) fn has_page(&self, page: u64) -> bool {
        self.checkpoint(page).is_some()
    }

    fn checkpoint(&self, page: u64) -> Option<Checkpoint> {
        self.checkpoints
            .lock_recover()
            .get(usize::try_from(page).ok()?)
            .copied()
    }

    /// Source rows are zero-based here. Used only with the unfiltered index.
    pub fn page_for_source_row(&self, row: u64) -> Option<u64> {
        if row >= self.total_rows.load(Ordering::Acquire) {
            return None;
        }
        let checkpoints = self.checkpoints.lock_recover();
        Some(
            checkpoints
                .partition_point(|point| point.source_line <= row)
                .saturating_sub(1) as u64,
        )
    }

    fn open_snapshot(&self) -> io::Result<File> {
        let file = File::open(&self.path)?;
        self.stamp.validate(&self.path, &file)?;
        Ok(file)
    }

    fn max_line_bytes(&self) -> usize {
        ((self.page_budget.saturating_sub(row_cost(0)) / 3).min(MAX_LINE_BYTES as u64)) as usize
    }

    fn fits_page(&self, rows: u64, bytes: u64, cost: u64) -> bool {
        rows < self.page_rows && cost <= self.page_budget.saturating_sub(bytes)
    }

    pub fn page_rows(&self) -> u64 {
        self.page_rows
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Drop for PagedIndex {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub struct PagedSource {
    pub base: Arc<PagedIndex>,
    pub filtered: Option<Arc<PagedIndex>>,
}

impl PagedSource {
    pub fn active(&self) -> &Arc<PagedIndex> {
        self.filtered.as_ref().unwrap_or(&self.base)
    }
}

impl Drop for PagedSource {
    fn drop(&mut self) {
        self.base.cancel();
        if let Some(filtered) = &self.filtered {
            filtered.cancel();
        }
    }
}

// Include the two per-row UI indexes; map/count overhead uses the other half
// of the log budget. Three bytes per input byte bounds lossy UTF-8 expansion.
fn row_cost(bytes: u64) -> u64 {
    bytes
        .saturating_mul(3)
        .saturating_add(crate::model::ENTRY_OVERHEAD + 8)
}

fn sample_page_rows(path: &Path, page_budget: u64) -> io::Result<u64> {
    let (bytes, lines) = sample_lines(path)?;
    let average = average_line_bytes(bytes, lines);
    let estimated_entry = row_cost(average);
    let rows = page_budget
        .checked_div(estimated_entry.max(1))
        .unwrap_or(CHECKPOINT_ROWS)
        .clamp(CHECKPOINT_ROWS, MAX_PAGE_ROWS);
    Ok((rows / CHECKPOINT_ROWS) * CHECKPOINT_ROWS)
}

fn sample_lines(path: &Path) -> io::Result<(u64, u64)> {
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 1024 * 1024];
    let mut bytes = 0u64;
    let mut lines = 0u64;
    while bytes < SAMPLE_BYTES {
        let want = (SAMPLE_BYTES - bytes).min(buffer.len() as u64) as usize;
        let n = file.read(&mut buffer[..want])?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        lines += buffer[..n].iter().filter(|&&b| b == b'\n').count() as u64;
    }
    Ok((bytes, lines))
}

fn average_line_bytes(bytes: u64, lines: u64) -> u64 {
    if lines == 0 {
        bytes.max(1)
    } else {
        bytes.div_ceil(lines)
    }
}

/// Decide whether full parsing is likely to consume too much RAM.
///
/// Estimate = raw text + LogEntry overhead + filtered index, then add 25% for
/// allocation rounding, maps/counts, and estimation error.
pub fn should_page(path: &Path, log_budget: u64) -> io::Result<bool> {
    let file_len = std::fs::metadata(path)?.len();
    if file_len < ESTIMATE_FROM_BYTES {
        return Ok(false);
    }
    if file_len >= FORCE_PAGING_BYTES {
        return Ok(true);
    }
    let (sample_bytes, sample_lines) = sample_lines(path)?;
    let average = average_line_bytes(sample_bytes, sample_lines);
    let estimated_lines = file_len.div_ceil(average);
    Ok(should_page_estimate(file_len, estimated_lines, log_budget))
}

fn should_page_estimate(file_len: u64, lines: u64, log_budget: u64) -> bool {
    let estimated = file_len
        .saturating_add(lines.saturating_mul(crate::model::ENTRY_OVERHEAD + 4))
        .saturating_mul(5)
        / 4;
    estimated > log_budget.saturating_mul(3) / 5
}

#[derive(Debug)]
pub struct Page {
    pub page: u64,
    pub total_rows: u64,
    pub entries: Vec<LogEntry>,
}

pub fn build_index(
    path: &Path,
    page_budget: u64,
    ctx: egui::Context,
) -> io::Result<Arc<PagedIndex>> {
    let stamp = FileStamp::read(path)?;
    let page_rows = sample_page_rows(path, page_budget)?;
    let index = PagedIndex::new(path.to_path_buf(), stamp, page_rows, page_budget);
    let worker = index.clone();
    std::thread::Builder::new()
        .name("large-file-index".into())
        .spawn(move || {
            if let Err(e) = scan_index(&worker, None, &ctx) {
                *worker.error.lock_recover() = Some(e.to_string());
                worker.complete.store(true, Ordering::Release);
                ctx.request_repaint();
            }
        })?;
    Ok(index)
}

pub fn build_filter_index(
    base: &Arc<PagedIndex>,
    spec: FilterSpec,
    ctx: egui::Context,
) -> io::Result<Arc<PagedIndex>> {
    let mut index = PagedIndex::new(
        base.path.clone(),
        base.stamp,
        base.page_rows,
        base.page_budget,
    );
    let state = Arc::get_mut(&mut index).unwrap();
    state.page_gate = base.page_gate.clone();
    state.filter_gate = base.filter_gate.clone();
    let worker = index.clone();
    std::thread::Builder::new()
        .name("large-file-filter".into())
        .spawn(move || {
            if let Err(e) = scan_index(&worker, Some(&spec), &ctx) {
                *worker.error.lock_recover() = Some(e.to_string());
                worker.complete.store(true, Ordering::Release);
                ctx.request_repaint();
            }
        })?;
    Ok(index)
}

fn scan_index(
    index: &Arc<PagedIndex>,
    spec: Option<&FilterSpec>,
    ctx: &egui::Context,
) -> io::Result<()> {
    let _filter_guard = spec.map(|_| index.filter_gate.lock_recover());
    let cancelled = || index.cancel.load(Ordering::Acquire);
    check_cancelled(cancelled())?;
    let file = index.open_snapshot()?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file.take(index.file_len));
    let bookmarks = HashSet::new();
    let mut raw = Vec::new();
    let mut offset = 0u64;
    let mut source_line = 0u64;
    let mut row = 0u64;
    let mut page_count = 0u64;
    let mut page_bytes = 0u64;
    let mut hint = LogFormat::Unknown;
    loop {
        let start = offset;
        let n = read_bounded_line(&mut reader, &mut raw, index.max_line_bytes(), &cancelled)?;
        if n == 0 {
            break;
        }
        offset += n as u64;
        source_line += 1;
        let matched = if let Some(spec) = spec {
            let text = decode_line(&raw, source_line == 1);
            let (entry, format) = parse_line_hinted(text, hint);
            if hint == LogFormat::Unknown && format != LogFormat::Unknown {
                hint = format;
            }
            spec.matches(&entry, 0, &bookmarks)
        } else {
            true
        };
        if matched {
            let cost = row_cost(n as u64);
            if cost > index.page_budget {
                return Err(io::Error::other("log line exceeds the page memory budget"));
            }
            if !index.fits_page(page_count, page_bytes, cost) {
                index.checkpoints.lock_recover().push(Checkpoint {
                    source_line: source_line - 1,
                    offset: start,
                });
                index.total_rows.store(row, Ordering::Release);
                page_count = 0;
                page_bytes = 0;
            }
            page_count += 1;
            page_bytes += cost;
            row += 1;
        }
        index.indexed_bytes.store(offset, Ordering::Release);
        if source_line.is_multiple_of(100_000) {
            index.total_rows.store(row, Ordering::Release);
            ctx.request_repaint();
        }
    }
    index
        .stamp
        .validate(&index.path, reader.get_ref().get_ref())?;
    if offset != index.file_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "paged snapshot was truncated",
        ));
    }
    index.total_rows.store(row, Ordering::Release);
    index.indexed_bytes.store(offset, Ordering::Release);
    index.complete.store(true, Ordering::Release);
    ctx.request_repaint();
    Ok(())
}

pub struct PageLoad {
    rx: crossbeam_channel::Receiver<io::Result<Page>>,
    cancel: Arc<AtomicBool>,
}

impl PageLoad {
    pub fn try_recv(&self) -> Result<io::Result<Page>, crossbeam_channel::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for PageLoad {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub fn load_page(
    index: Arc<PagedIndex>,
    page: u64,
    spec: Option<FilterSpec>,
    ctx: egui::Context,
) -> io::Result<PageLoad> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    std::thread::Builder::new()
        .name("large-file-page".into())
        .spawn(move || {
            let _guard = index.page_gate.lock_recover();
            let result = load_page_cancellable(&index, page, spec.as_ref(), &worker_cancel);
            let _ = tx.send(result);
            ctx.request_repaint();
        })?;
    Ok(PageLoad { rx, cancel })
}

#[cfg(test)]
fn load_page_sync(index: &PagedIndex, page: u64, spec: Option<&FilterSpec>) -> io::Result<Page> {
    load_page_cancellable(index, page, spec, &AtomicBool::new(false))
}

fn check_cancelled(cancelled: bool) -> io::Result<()> {
    if cancelled {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "paged request cancelled",
        ))
    } else {
        Ok(())
    }
}

fn load_page_cancellable(
    index: &PagedIndex,
    page: u64,
    spec: Option<&FilterSpec>,
    request_cancel: &AtomicBool,
) -> io::Result<Page> {
    let cancelled =
        || index.cancel.load(Ordering::Acquire) || request_cancel.load(Ordering::Acquire);
    check_cancelled(cancelled())?;
    let file = index.open_snapshot()?;
    let checkpoint = index
        .checkpoint(page)
        .ok_or_else(|| io::Error::other("page is not indexed yet"))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file.take(index.file_len));
    reader
        .get_mut()
        .get_mut()
        .seek(SeekFrom::Start(checkpoint.offset))?;
    reader
        .get_mut()
        .set_limit(index.file_len - checkpoint.offset);
    let bookmarks = HashSet::new();
    let mut raw = Vec::new();
    let mut source_line = checkpoint.source_line;
    let mut bytes = 0u64;
    let mut entries = Vec::new();
    let mut hint = LogFormat::Unknown;
    while entries.len() < index.page_rows as usize {
        let n = read_bounded_line(&mut reader, &mut raw, index.max_line_bytes(), &cancelled)?;
        if n == 0 {
            break;
        }
        source_line += 1;
        let text = decode_line(&raw, source_line == 1);
        let (mut entry, format) = parse_line_hinted(text, hint);
        if hint == LogFormat::Unknown && format != LogFormat::Unknown {
            hint = format;
        }
        let matched = spec.is_none_or(|s| s.matches(&entry, 0, &bookmarks));
        if matched {
            let cost = row_cost(n as u64);
            if cost > index.page_budget {
                return Err(io::Error::other("log line exceeds the page memory budget"));
            }
            if !index.fits_page(entries.len() as u64, bytes, cost) {
                break;
            }
            // Reserve only a bounded chunk, rather than a multi-million-row
            // vector up front (especially wasteful for sparse filter results).
            if entries.len() == entries.capacity() {
                entries.try_reserve_exact(256).map_err(|_| {
                    io::Error::new(io::ErrorKind::OutOfMemory, "cannot allocate paged entries")
                })?;
            }
            bytes += cost;
            entry.line_no = source_line;
            entries.push(entry);
        }
    }
    check_cancelled(cancelled())?;
    index
        .stamp
        .validate(&index.path, reader.get_ref().get_ref())?;
    Ok(Page {
        page,
        total_rows: index.total_rows.load(Ordering::Acquire),
        entries,
    })
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> io::Result<usize> {
    output.clear();
    let mut total = 0usize;
    loop {
        check_cancelled(cancelled())?;
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total);
        }
        let take = available
            .iter()
            .position(|&b| b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if total.saturating_add(take) > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                if max_bytes < MAX_LINE_BYTES {
                    "log line exceeds the page memory budget"
                } else {
                    "log line exceeds the 64 MiB paged-view limit"
                },
            ));
        }
        if total + take > output.capacity() {
            let capacity = output
                .capacity()
                .saturating_mul(2)
                .max(total + take)
                .min(max_bytes);
            output
                .try_reserve_exact(capacity - output.len())
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::OutOfMemory, "cannot allocate paged log line")
                })?;
        }
        output.extend_from_slice(&available[..take]);
        reader.consume(take);
        total += take;
        if output.last() == Some(&b'\n') {
            return Ok(total);
        }
    }
}

fn decode_line(raw: &[u8], first: bool) -> String {
    let line = String::from_utf8_lossy(raw)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if first {
        line.strip_prefix('\u{feff}').unwrap_or(&line).to_string()
    } else {
        line
    }
}

pub fn export_filtered(
    index: &PagedIndex,
    destination: &Path,
    spec: &FilterSpec,
) -> io::Result<u64> {
    let file = index.open_snapshot()?;
    // create_new also rejects aliases/hardlinks to the source, without truncating.
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut writer = std::io::BufWriter::new(output);
    let result = export_to(index, file, &mut writer, spec);
    let result = result.and_then(|rows| {
        writer.get_ref().sync_all()?;
        Ok(rows)
    });
    drop(writer);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

fn export_to(
    index: &PagedIndex,
    file: File,
    writer: &mut impl io::Write,
    spec: &FilterSpec,
) -> io::Result<u64> {
    let mut reader = BufReader::with_capacity(1024 * 1024, file.take(index.file_len));
    let bookmarks = HashSet::new();
    let mut raw = Vec::new();
    let mut hint = LogFormat::Unknown;
    let mut rows = 0u64;
    let mut source_line = 0u64;
    loop {
        // An explicitly requested export may finish after the view is closed.
        let n = read_bounded_line(&mut reader, &mut raw, index.max_line_bytes(), &|| false)?;
        if n == 0 {
            break;
        }
        source_line += 1;
        let text = decode_line(&raw, source_line == 1);
        let (entry, format) = parse_line_hinted(text, hint);
        if hint == LogFormat::Unknown && format != LogFormat::Unknown {
            hint = format;
        }
        if spec.matches(&entry, 0, &bookmarks) {
            writer.write_all(entry.raw().as_bytes())?;
            writer.write_all(b"\n")?;
            rows += 1;
        }
    }
    index
        .stamp
        .validate(&index.path, reader.get_ref().get_ref())?;
    if reader.get_ref().limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "paged snapshot was truncated",
        ));
    }
    writer.flush()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file(lines: usize) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "lf_paged_{}_{}.log",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = File::create(&path).unwrap();
        for i in 0..lines {
            writeln!(f, "01-01 00:00:00.000  1  2 I Tag{}: message {i}", i % 3).unwrap();
        }
        path
    }

    fn wait(index: &PagedIndex) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !index.complete.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn sparse_index_and_pages_cover_large_line_counts() {
        let path = file(CHECKPOINT_ROWS as usize * 3 + 17);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        assert_eq!(
            index.total_rows.load(Ordering::Acquire),
            CHECKPOINT_ROWS * 3 + 17
        );
        assert_eq!(index.checkpoints.lock_recover().len(), 4);
        let page = load_page_sync(&index, 2, None).unwrap();
        assert_eq!(page.entries.len(), CHECKPOINT_ROWS as usize);
        assert_eq!(page.entries[0].line_no, CHECKPOINT_ROWS * 2 + 1);
        let last = load_page_sync(&index, 3, None).unwrap();
        assert_eq!(last.entries.len(), 17);
        assert_eq!(
            last.entries.last().unwrap().line_no,
            CHECKPOINT_ROWS * 3 + 17
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn filtered_index_pages_keep_original_line_numbers() {
        let path = file(CHECKPOINT_ROWS as usize * 4);
        let base = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&base);
        let spec = FilterSpec {
            allowed_tags: Some(["Tag1".to_string()].into()),
            ..Default::default()
        };
        let index = build_filter_index(&base, spec.clone(), egui::Context::default()).unwrap();
        wait(&index);
        assert_eq!(
            index.total_rows.load(Ordering::Acquire),
            (CHECKPOINT_ROWS * 4 + 1) / 3
        );
        let page = load_page_sync(&index, 1, Some(&spec)).unwrap();
        assert!(!page.entries.is_empty());
        assert!(page.entries.iter().all(|e| e.tag() == "Tag1"));
        assert!(page.entries[0].line_no > CHECKPOINT_ROWS);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn page_load_rejects_a_file_changed_after_indexing() {
        let path = file(100);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"changed\n")
            .unwrap();
        assert!(load_page_sync(&index, 0, None)
            .unwrap_err()
            .to_string()
            .contains("changed"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sparse_index_memory_scales_with_pages_not_rows() {
        let path = file(CHECKPOINT_ROWS as usize * 5 + 1);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        assert_eq!(index.checkpoints.lock_recover().len(), 6);
        assert!(
            index.checkpoints.lock_recover().capacity() * std::mem::size_of::<Checkpoint>() < 4096
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn page_size_uses_more_rows_when_the_memory_budget_is_larger() {
        let path = file(10_000);
        let small = sample_page_rows(&path, 4 * 1024 * 1024).unwrap();
        let large = sample_page_rows(&path, 1024 * 1024 * 1024).unwrap();
        assert_eq!(small, CHECKPOINT_ROWS);
        assert!(large > small);
        assert!(large <= MAX_PAGE_ROWS);
        assert!(large.is_multiple_of(CHECKPOINT_ROWS));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn paging_estimate_accounts_for_short_line_overhead_and_ram() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // Two GiB of 50-byte rows carries more than five GiB of entry/index
        // overhead, so even a 16 GiB-available machine should page it.
        assert!(should_page_estimate(2 * GIB, 2 * GIB / 50, 8 * GIB));
        // The same file with 4 KiB rows fits comfortably in the same budget.
        assert!(!should_page_estimate(2 * GIB, 2 * GIB / 4096, 8 * GIB));
        // Larger memory permits the short-row file to load whole.
        assert!(!should_page_estimate(2 * GIB, 2 * GIB / 50, 32 * GIB));
    }

    #[test]
    fn paging_has_small_file_and_hard_large_file_bounds() {
        let small = std::env::temp_dir().join(format!("lf_page_small_{}.log", std::process::id()));
        std::fs::write(&small, b"x\n").unwrap();
        assert!(!should_page(&small, 1).unwrap());
        std::fs::remove_file(small).unwrap();

        let huge = std::env::temp_dir().join(format!("lf_page_huge_{}.log", std::process::id()));
        File::create(&huge)
            .unwrap()
            .set_len(FORCE_PAGING_BYTES)
            .unwrap();
        assert!(should_page(&huge, u64::MAX).unwrap());
        std::fs::remove_file(huge).unwrap();
    }

    #[test]
    fn filtered_export_scans_rows_outside_the_loaded_page() {
        let path = file(CHECKPOINT_ROWS as usize + 25);
        let destination = path.with_extension("filtered");
        let spec = FilterSpec {
            find: vec![format!("message {}", CHECKPOINT_ROWS + 20)],
            ..Default::default()
        };
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        assert_eq!(export_filtered(&index, &destination, &spec).unwrap(), 1);
        let text = std::fs::read_to_string(&destination).unwrap();
        assert!(text.contains(&format!("message {}", CHECKPOINT_ROWS + 20)));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(destination).unwrap();
    }

    #[test]
    fn long_rows_after_the_sample_obey_budget_and_keep_every_source_line() {
        let path = file(0);
        let mut output = io::BufWriter::new(File::create(&path).unwrap());
        // Entire 8 MiB sample contains short rows; later pages contain long rows.
        for _ in 0..280_000 {
            output
                .write_all(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n")
                .unwrap();
        }
        let long = "y".repeat(1023) + "\n";
        for _ in 0..20_000 {
            output.write_all(long.as_bytes()).unwrap();
        }
        output.flush().unwrap();
        drop(output);
        let budget = 4 * 1024 * 1024;
        let index = build_index(&path, budget, egui::Context::default()).unwrap();
        wait(&index);
        assert!(index.error.lock_recover().is_none());
        let mut next_line = 1;
        for number in 0..index.pages() {
            let page = load_page_sync(&index, number, None).unwrap();
            assert!(!page.entries.is_empty());
            assert!(
                page.entries
                    .iter()
                    .map(LogEntry::estimated_bytes)
                    .sum::<u64>()
                    <= budget
            );
            for entry in page.entries {
                assert_eq!(entry.line_no, next_line);
                assert_eq!(index.page_for_source_row(next_line - 1), Some(number));
                next_line += 1;
            }
        }
        assert_eq!(next_line, 300_001);
        assert!(index.page_for_source_row(300_000).is_none());
        let spec = FilterSpec {
            find: vec!["y".into()],
            ..Default::default()
        };
        let filtered = build_filter_index(&index, spec.clone(), egui::Context::default()).unwrap();
        wait(&filtered);
        assert!(filtered.error.lock_recover().is_none());
        let mut next_line = 280_001;
        for number in 0..filtered.pages() {
            let page = load_page_sync(&filtered, number, Some(&spec)).unwrap();
            assert!(
                page.entries
                    .iter()
                    .map(LogEntry::estimated_bytes)
                    .sum::<u64>()
                    <= budget
            );
            for entry in page.entries {
                assert_eq!(entry.line_no, next_line);
                assert_eq!(entry.raw(), long.trim_end());
                next_line += 1;
            }
        }
        assert_eq!(next_line, 300_001);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn byte_pages_account_for_lossy_utf8_and_reject_a_single_oversized_row() {
        let path = file(0);
        std::fs::write(
            &path,
            [vec![0xff; 1000], b"\n".to_vec()].concat().repeat(10),
        )
        .unwrap();
        let index = build_index(&path, 4000, egui::Context::default()).unwrap();
        wait(&index);
        assert!(index.error.lock_recover().is_none());
        assert_eq!(index.pages(), 10);
        for number in 0..10 {
            let page = load_page_sync(&index, number, None).unwrap();
            assert_eq!(page.entries.len(), 1);
            assert_eq!(page.entries[0].raw().len(), 3000);
            assert!(page.entries[0].estimated_bytes() <= 4000);
        }
        let too_small = build_index(&path, 1000, egui::Context::default()).unwrap();
        wait(&too_small);
        assert!(too_small
            .error
            .lock_recover()
            .as_ref()
            .unwrap()
            .contains("memory budget"));
        assert!(load_page_sync(&too_small, 0, None).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_and_replaced_page_requests_cancel_before_reading() {
        let path = file(100);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        // Deterministically hold both requests before they may allocate/read.
        let gate = index.page_gate.lock_recover();
        let old = load_page(index.clone(), 0, None, egui::Context::default()).unwrap();
        let old_cancel = old.cancel.clone();
        drop(old);
        let latest = load_page(index.clone(), 0, None, egui::Context::default()).unwrap();
        assert!(old_cancel.load(Ordering::Acquire));
        drop(gate);
        let page = latest
            .rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(page.entries.len(), 100);
        index.cancel();
        assert_eq!(
            load_page_sync(&index, 0, None).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancelling_during_a_long_line_stops_at_the_next_io_block() {
        struct CancellingReader<'a> {
            cancel: &'a AtomicBool,
            block: [u8; 1024],
        }
        impl Read for CancellingReader<'_> {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                unreachable!()
            }
        }
        impl BufRead for CancellingReader<'_> {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                self.cancel.store(true, Ordering::Release);
                Ok(&self.block)
            }
            fn consume(&mut self, _: usize) {}
        }
        let cancel = AtomicBool::new(false);
        let mut reader = CancellingReader {
            cancel: &cancel,
            block: [b'x'; 1024],
        };
        let mut raw = Vec::new();
        let error = read_bounded_line(&mut reader, &mut raw, MAX_LINE_BYTES, &|| {
            cancel.load(Ordering::Acquire)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(raw.len(), 1024);
    }

    #[test]
    fn export_rejects_mutations_before_and_during_reading() {
        let path = file(100);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        let destination = path.with_extension("export");
        let spec = FilterSpec::default();
        assert!(export_filtered(&index, &path, &spec).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), index.file_len);
        let handle = index.open_snapshot().unwrap();
        struct MutatingWriter<'a> {
            path: &'a Path,
            changed: bool,
        }
        impl Write for MutatingWriter<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.changed {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(self.path)?
                        .write_all(b"appended\n")?;
                    self.changed = true;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = MutatingWriter {
            path: &path,
            changed: false,
        };
        assert!(export_to(&index, handle, &mut writer, &spec)
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert!(export_filtered(&index, &destination, &spec).is_err());
        assert!(!destination.exists());
        assert!(load_page_sync(&index, 0, None).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_detects_replacement_even_with_identical_length_and_timestamp() {
        let path = file(10);
        let stamp = FileStamp::read(&path).unwrap();
        let held = File::open(&path).unwrap();
        let replacement = file(11);
        let old = path.with_extension("old");
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_len(stamp.len)
            .unwrap();
        let times = std::fs::FileTimes::new().set_modified(stamp.modified.unwrap());
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(times)
            .unwrap();
        std::fs::rename(&path, &old).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(stamp.validate(&path, &held).is_err());
        drop(held);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(old).unwrap();
    }

    #[test]
    fn failed_export_removes_partial_output_and_preserves_existing_destinations() {
        let path = file(1);
        let source = File::options().write(true).open(&path).unwrap();
        source.set_len(MAX_LINE_BYTES as u64 + 1024).unwrap();
        drop(source);
        let index = PagedIndex::new(
            path.clone(),
            FileStamp::read(&path).unwrap(),
            CHECKPOINT_ROWS,
            256 * 1024 * 1024,
        );
        let destination = path.with_extension("export");
        assert!(export_filtered(&index, &destination, &FilterSpec::default()).is_err());
        assert!(!destination.exists());
        std::fs::write(&destination, b"existing output").unwrap();
        assert!(export_filtered(&index, &destination, &FilterSpec::default()).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing output");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(destination).unwrap();
    }
}
