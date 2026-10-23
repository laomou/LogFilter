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
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub const CHECKPOINT_ROWS: u64 = 20_000;
pub const ESTIMATE_FROM_BYTES: u64 = 256 * 1024 * 1024;
pub const FORCE_PAGING_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PAGE_ROWS: u64 = 5_000_000;
/// How much of *each end* of a file to read when estimating its mean line
/// length.
///
/// These reads happen on the UI thread (the open path validates access and
/// makes the paged/in-memory decision before handing off to a worker), so
/// each is kept small: a few thousand lines already pin the mean of its
/// region, and on slow or spun-down media every extra megabyte is a visible
/// freeze.
///
/// Both ends are sampled because a leading region alone cannot represent a
/// file whose line lengths shift partway through, and `should_page` has no
/// later correction: `fits_page` can shrink an over-large page while
/// scanning, but nothing can bail out of an in-memory load that turned out
/// not to fit. The estimator therefore takes the *pessimistic* (smallest)
/// mean across the sampled regions — a short-line region anywhere in the
/// sampled ends biases the decision toward paging, which is always the safe
/// direction.
const SAMPLE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy)]
struct Checkpoint {
    source_line: u64,
    offset: u64,
}

/// The file on disk no longer matches the indexed snapshot.
///
/// A distinct type (rather than a message string) so the viewer can tell "the
/// file changed, rebuild the index" apart from any other read failure.
#[derive(Debug)]
pub struct FileChangedError;

impl std::fmt::Display for FileChangedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("file changed while the paged snapshot was open; reopen it")
    }
}

impl std::error::Error for FileChangedError {}

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
            Err(io::Error::other(FileChangedError))
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

// Note: there is deliberately no `Drop` here that cancels the scan. Every
// scan/page worker holds its own `Arc<PagedIndex>` clone, so a drop could only
// run after those workers are already gone — the cancel would be a no-op that
// merely implies (wrongly) that dropping the last reference stops a running
// scan. Cancellation is driven explicitly, from `PagedSource::drop`.

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

/// Read the first `SAMPLE_BYTES` of the file and, when the file is longer
/// than twice that, the last `SAMPLE_BYTES` as well.
///
/// Returns `[(head_bytes, head_lines), (tail_bytes, tail_lines)]`. For short
/// files both entries describe the same sampled region, so the pessimistic
/// mean below degrades gracefully to the whole-file sample.
fn sample_regions(path: &Path, file_len: u64) -> io::Result<[(u64, u64); 2]> {
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    let read = |file: &mut File, buffer: &mut [u8], want: u64| -> io::Result<(u64, u64)> {
        let mut bytes = 0u64;
        let mut lines = 0u64;
        while bytes < want {
            let want = (want - bytes).min(buffer.len() as u64) as usize;
            let n = file.read(&mut buffer[..want])?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            lines += buffer[..n].iter().filter(|&&b| b == b'\n').count() as u64;
        }
        Ok((bytes, lines))
    };
    let head = read(&mut file, &mut buffer, file_len.min(SAMPLE_BYTES))?;
    let tail = if file_len > 2 * SAMPLE_BYTES {
        file.seek(SeekFrom::Start(file_len - SAMPLE_BYTES))?;
        read(&mut file, &mut buffer, SAMPLE_BYTES)?
    } else {
        head
    };
    Ok([head, tail])
}

/// The pessimistic (smallest) mean line length across the sampled regions.
///
/// A smaller mean estimates more rows, and more rows estimate more memory, so
/// both consumers err toward the safe side: `should_page` toward paging, and
/// `sample_page_rows` toward smaller pages (which `fits_page` re-checks
/// against the real byte budget while scanning). The tail sample can start
/// mid-line; its truncated first fragment lowers that region's mean slightly,
/// which also errs conservatively.
fn conservative_average_line_bytes(regions: &[(u64, u64); 2]) -> u64 {
    regions
        .iter()
        .map(|&(bytes, lines)| average_line_bytes(bytes, lines))
        .min()
        .unwrap_or(1)
}

fn sample_page_rows(path: &Path, file_len: u64, page_budget: u64) -> io::Result<u64> {
    let average = conservative_average_line_bytes(&sample_regions(path, file_len)?);
    let estimated_entry = row_cost(average);
    let rows = page_budget
        .checked_div(estimated_entry.max(1))
        .unwrap_or(CHECKPOINT_ROWS)
        .clamp(CHECKPOINT_ROWS, MAX_PAGE_ROWS);
    Ok((rows / CHECKPOINT_ROWS) * CHECKPOINT_ROWS)
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
    let average = conservative_average_line_bytes(&sample_regions(path, file_len)?);
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
    let page_rows = sample_page_rows(path, stamp.len, page_budget)?;
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
        let n = crate::io::read_line_bounded(
            &mut reader,
            &mut raw,
            index.max_line_bytes(),
            &cancelled,
            line_too_long_message(index.max_line_bytes()),
        )?;
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
    // A cleanly completed index with zero matching rows (or an empty file)
    // has nothing to read: return the empty page instead of re-scanning the
    // whole snapshot on every request — with a filtered view that would be
    // O(file) per page. The stamp was just validated by `open_snapshot`, so a
    // changed file still surfaces as a `FileChangedError`. A scan that *failed*
    // (e.g. a row beyond the page budget) is not zero-row: it must keep
    // reporting its error.
    if index.complete.load(Ordering::Acquire)
        && index.error.lock_recover().is_none()
        && index.total_rows.load(Ordering::Acquire) == 0
    {
        return Ok(Page {
            page,
            total_rows: 0,
            entries: Vec::new(),
        });
    }
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
        let n = crate::io::read_line_bounded(
            &mut reader,
            &mut raw,
            index.max_line_bytes(),
            &cancelled,
            line_too_long_message(index.max_line_bytes()),
        )?;
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

/// Why a line was refused, depending on which cap applied.
fn line_too_long_message(max_bytes: usize) -> &'static str {
    if max_bytes < MAX_LINE_BYTES {
        "log line exceeds the page memory budget"
    } else {
        "log line exceeds the 64 MiB paged-view limit"
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

#[cfg(test)]
pub fn export_filtered(
    index: &PagedIndex,
    destination: &Path,
    spec: &FilterSpec,
) -> io::Result<u64> {
    export_with_bookmarks(index, destination, spec, &HashSet::new())
}

pub fn export_with_bookmarks(
    index: &PagedIndex,
    destination: &Path,
    spec: &FilterSpec,
    bookmarked_lines: &HashSet<u64>,
) -> io::Result<u64> {
    let file = index.open_snapshot()?;
    // Never write over the file being exported from, through any path or link to
    // it. (The save dialog already asked the user before offering an existing
    // name, so other existing destinations are replaced.)
    if let Ok(existing) = File::open(destination) {
        if crate::io::same_file(&file, &existing)? {
            return Err(io::Error::other("cannot overwrite the file being exported"));
        }
    }
    // Stream into a sibling temp file and rename it into place only on success:
    // a failed export must leave an existing destination intact, and a crash must
    // not expose a half-written export under the user's chosen name.
    let tmp = crate::config::temp_sibling(destination);
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    let mut writer = std::io::BufWriter::new(output);
    let result = export_to_bookmarked(index, file, &mut writer, spec, bookmarked_lines);
    let result = result.and_then(|rows| {
        writer.get_ref().sync_all()?;
        Ok(rows)
    });
    drop(writer);
    match result {
        Ok(rows) => match std::fs::rename(&tmp, destination) {
            Ok(()) => {
                crate::config::sync_parent_dir(destination);
                Ok(rows)
            }
            Err(e) => {
                // The rows were written but could not be published: destination
                // is a directory, not writable, on another device (EXDEV), or
                // held open by another process on Windows. Don't leave the
                // sibling temp behind for the user to find and wonder about.
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        },
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
fn export_to(
    index: &PagedIndex,
    file: File,
    writer: &mut impl io::Write,
    spec: &FilterSpec,
) -> io::Result<u64> {
    export_to_bookmarked(index, file, writer, spec, &HashSet::new())
}

fn export_to_bookmarked(
    index: &PagedIndex,
    file: File,
    writer: &mut impl io::Write,
    spec: &FilterSpec,
    bookmarked_lines: &HashSet<u64>,
) -> io::Result<u64> {
    let mut reader = BufReader::with_capacity(1024 * 1024, file.take(index.file_len));
    let bookmarks = HashSet::new();
    let mut disk_spec = spec.clone();
    disk_spec.bookmarks_only = false;
    let mut raw = Vec::new();
    let mut hint = LogFormat::Unknown;
    let mut rows = 0u64;
    let mut source_line = 0u64;
    loop {
        // An explicitly requested export may finish after the view is closed.
        let n = crate::io::read_line_bounded(
            &mut reader,
            &mut raw,
            index.max_line_bytes(),
            &|| false,
            line_too_long_message(index.max_line_bytes()),
        )?;
        if n == 0 {
            break;
        }
        source_line += 1;
        let text = decode_line(&raw, source_line == 1);
        let (entry, format) = parse_line_hinted(text, hint);
        if hint == LogFormat::Unknown && format != LogFormat::Unknown {
            hint = format;
        }
        if (!spec.bookmarks_only || bookmarked_lines.contains(&source_line))
            && disk_spec.matches(&entry, 0, &bookmarks)
        {
            // Match on the decoded text but write the *original bytes*, exactly
            // as `recording::export` does: an export is a copy of the matching
            // rows, so invalid UTF-8, CRLF endings and any BOM must survive it.
            // `entry.raw()` is `decode_line` output, which would replace invalid
            // bytes with U+FFFD, normalise CRLF to LF, drop the BOM and strip a
            // trailing '\r' that belonged to the data. `raw` already carries its
            // line terminator, and only the final line can lack one, so no rows
            // can run together.
            writer.write_all(&raw)?;
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
    use std::io::{BufRead, Write};

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
        // Indexing here is a correctness prerequisite, not a speed benchmark.
        // Shared Windows CI disks can be busy with the concurrent 300 MiB
        // stress fixtures; keep a finite deadlock bound without requiring a
        // large scan to finish in five seconds on every runner.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !index.complete.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "indexing timed out for {}: {} / {} bytes, {} rows, error: {:?}",
                index.path.display(),
                index.indexed_bytes.load(Ordering::Acquire),
                index.file_len,
                index.total_rows.load(Ordering::Acquire),
                index.error.lock_recover().as_deref(),
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn bookmark_export_includes_lines_from_multiple_pages() {
        let path = file(CHECKPOINT_ROWS as usize + 5);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        let destination = path.with_extension("bookmarks");
        let lines = HashSet::from([1, CHECKPOINT_ROWS + 3]);
        let mut spec = FilterSpec {
            bookmarks_only: true,
            ..Default::default()
        };
        assert_eq!(
            export_with_bookmarks(&index, &destination, &spec, &lines).unwrap(),
            2
        );
        let output = std::fs::read_to_string(&destination).unwrap();
        assert!(output.contains("message 0\n"));
        assert!(output.contains(&format!("message {}\n", CHECKPOINT_ROWS + 2)));
        spec.find = vec![format!("message {}", CHECKPOINT_ROWS + 2)];
        assert_eq!(
            export_with_bookmarks(&index, &destination, &spec, &lines).unwrap(),
            1
        );
        std::fs::remove_file(destination).unwrap();
        std::fs::remove_file(path).unwrap();
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
    fn zero_row_completed_index_returns_an_empty_page_without_rescanning() {
        // A filter that matches nothing must not pay a full-file re-read per
        // page request once the index is complete.
        let path = file(10_000);
        let base = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&base);
        let spec = FilterSpec {
            allowed_tags: Some(["NoSuchTag".to_string()].into()),
            ..Default::default()
        };
        let index = build_filter_index(&base, spec.clone(), egui::Context::default()).unwrap();
        wait(&index);
        assert_eq!(index.total_rows.load(Ordering::Acquire), 0);
        let page = load_page_sync(&index, 0, Some(&spec)).unwrap();
        assert!(page.entries.is_empty());
        assert_eq!(page.total_rows, 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn page_loads_parse_with_the_hint_the_scan_latched() {
        // bugreport-style mix: the file starts with a threadtime line (which
        // latches the filter scan's hint), 20 000 kernel lines then fill page 0,
        // and the final line is threadtime again. Page 1 begins on a kernel
        // line and re-latches its hint from that line, while the scan latched
        // its hint from line 1. The page must still parse its trailing
        // threadtime row identically to the scan: the parse has to stay
        // hint-independent (the kernel arm rejects lines without `<pri>`/`[ts]`
        // and every hint order tries HiLog before ThreadTime), or tag/level
        // filters would disagree with the scan's matched set (missing or
        // duplicated rows, wrong total_rows).
        let path = file(0);
        let mut body = String::new();
        body.push_str("07-20 12:00:00.000  1  2 I First: hello zz\n");
        for _ in 0..CHECKPOINT_ROWS {
            body.push_str("<6>[    1.000] kernel line zz\n");
        }
        body.push_str("07-20 12:00:01.000  1  2 I First: again zz\n");
        std::fs::write(&path, body).unwrap();
        let base = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&base);
        let spec = FilterSpec {
            find: vec!["zz".to_string()],
            ..Default::default()
        };
        let index = build_filter_index(&base, spec.clone(), egui::Context::default()).unwrap();
        wait(&index);
        // Every row matches the find filter, so the scan counts all of them and
        // the page boundary lands on a kernel line.
        assert_eq!(
            index.total_rows.load(Ordering::Acquire),
            CHECKPOINT_ROWS + 2
        );
        assert_eq!(index.pages(), 2, "fixture must span two pages");
        let page = load_page_sync(&index, 1, Some(&spec)).unwrap();
        let last = page.entries.last().expect("page 1 holds the final row");
        assert_eq!(last.line_no, CHECKPOINT_ROWS + 2);
        assert_eq!(
            last.tag(),
            "First",
            "final row must keep its threadtime parse, not degrade to a kernel row"
        );
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
        let len = std::fs::metadata(&path).unwrap().len();
        let small = sample_page_rows(&path, len, 4 * 1024 * 1024).unwrap();
        let large = sample_page_rows(&path, len, 1024 * 1024 * 1024).unwrap();
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

    // -- Stress tests: line-length distributions that shift across the file. --
    //
    // `should_page` decides on the UI thread with no later correction for a
    // wrong in-memory call, so these build real files above
    // `ESTIMATE_FROM_BYTES` whose leading region misrepresents the rest and
    // check the decision against the ground-truth row count.

    /// 300 MiB: above `ESTIMATE_FROM_BYTES`, far below `FORCE_PAGING_BYTES`.
    const STRESS_FILE_BYTES: u64 = 300 * 1024 * 1024;
    const STRESS_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

    fn stress_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lf_paged_stress_{name}_{}.log", std::process::id()))
    }

    /// Fill `region_len` bytes with `line_len`-byte lines of `filler` padding.
    /// The last piece may be shorter than `line_len` and ends without a
    /// newline, so regions may end mid-line.
    fn write_region(f: &mut File, region_len: u64, line_len: u64, filler: u8) {
        let mut line = vec![filler; line_len as usize];
        line[line_len as usize - 1] = b'\n';
        let mut written = 0u64;
        while written < region_len {
            let take = (region_len - written).min(line_len) as usize;
            f.write_all(&line[..take]).unwrap();
            written += take as u64;
        }
    }

    #[test]
    fn conservative_average_prefers_the_smaller_region_mean() {
        // 8 KiB rows vs 50-byte rows + a 44-byte fragment, either order.
        // div_ceil(262_144, 5_242) = 51: the fragment lifts the region mean.
        assert_eq!(
            conservative_average_line_bytes(&[(262_144, 32), (262_144, 5_242)]),
            51
        );
        assert_eq!(
            conservative_average_line_bytes(&[(262_144, 5_242), (262_144, 32)]),
            51
        );
        assert_eq!(conservative_average_line_bytes(&[(100, 2), (100, 2)]), 50);
        // A region with no newline at all is one (partial) line.
        assert_eq!(conservative_average_line_bytes(&[(0, 0), (262_144, 32)]), 1);
    }

    #[test]
    fn sample_regions_covers_both_ends() {
        let path = stress_path("two_ends");
        let mut f = File::create(&path).unwrap();
        write_region(&mut f, SAMPLE_BYTES, 50, b'a'); // 5242 rows + 44-byte fragment
        write_region(&mut f, 524_288, 50, b'b'); // middle
        write_region(&mut f, SAMPLE_BYTES, 8192, b'c'); // 32 rows of 8 KiB
        f.sync_all().unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 1_048_576);
        let regions = sample_regions(&path, len).unwrap();
        assert_eq!(regions[0], (SAMPLE_BYTES, 5_242));
        assert_eq!(regions[1], (SAMPLE_BYTES, 32));
        // The 44-byte fragment at the end of the head region lifts its mean
        // from 50 to div_ceil(262_144, 5_242) = 51.
        assert_eq!(conservative_average_line_bytes(&regions), 51);
        // Short files cannot sample two distinct ends: both entries describe
        // the same region.
        let short = file(100);
        let short_len = std::fs::metadata(&short).unwrap().len();
        let short_regions = sample_regions(&short, short_len).unwrap();
        assert_eq!(short_regions[0], short_regions[1]);
        std::fs::remove_file(&short).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn should_page_survives_a_long_head_before_a_short_body() {
        // The dangerous misread: a verbose leading region makes a head-only
        // sample over-estimate the mean line length, under-estimate the row
        // count, and call a file that does not fit the budget "safe" to load
        // whole.
        let path = stress_path("long_head");
        let head_lines = SAMPLE_BYTES / 8192; // exactly 32
        let mut f = File::create(&path).unwrap();
        write_region(&mut f, SAMPLE_BYTES, 8192, b'x');
        write_region(&mut f, STRESS_FILE_BYTES - SAMPLE_BYTES, 50, b'y');
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, STRESS_FILE_BYTES);
        // Ground truth: the body's 50-byte rows dominate the row count.
        let true_lines = head_lines + (STRESS_FILE_BYTES - SAMPLE_BYTES).div_ceil(50);
        assert!(
            should_page_estimate(len, true_lines, STRESS_BUDGET),
            "test setup: the ground truth must not fit the budget"
        );
        assert!(
            should_page(&path, STRESS_BUDGET).unwrap(),
            "a short-line body must not be hidden by a long head"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn should_page_is_conservative_for_a_short_head_before_a_long_body() {
        // The safe misread: a terse leading region under-estimates the mean
        // line length. Paging here costs nothing (the file fits), so the
        // pessimistic mean should still bias the decision toward paging.
        let path = stress_path("short_head");
        let mut f = File::create(&path).unwrap();
        // 5242 rows of 50 bytes plus a 44-byte fragment fill the head region.
        write_region(&mut f, SAMPLE_BYTES, 50, b'y');
        write_region(&mut f, STRESS_FILE_BYTES - SAMPLE_BYTES, 8192, b'x');
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, STRESS_FILE_BYTES);
        let true_lines = SAMPLE_BYTES.div_ceil(50) + (STRESS_FILE_BYTES - SAMPLE_BYTES) / 8192;
        assert!(
            !should_page_estimate(len, true_lines, STRESS_BUDGET),
            "test setup: the ground truth fits, so paging here is pure conservatism"
        );
        assert!(
            should_page(&path, STRESS_BUDGET).unwrap(),
            "the pessimistic mean must still bias toward paging"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn should_page_agrees_with_the_truth_on_uniform_files() {
        // Uniform line lengths: head and tail means coincide, so the
        // pessimistic estimator must reproduce the decision exactly.
        let path = stress_path("uniform");
        let mut f = File::create(&path).unwrap();
        write_region(&mut f, STRESS_FILE_BYTES, 50, b'z');
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, STRESS_FILE_BYTES);
        let true_lines = len.div_ceil(50);
        let truth = should_page_estimate(len, true_lines, STRESS_BUDGET);
        assert_eq!(
            should_page(&path, STRESS_BUDGET).unwrap(),
            truth,
            "uniform files must be decided from the sampled mean alone"
        );
        std::fs::remove_file(path).unwrap();
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

    /// An export is a copy of the matching rows, so it must be byte-faithful.
    ///
    /// This path used to write `entry.raw()`, the decoded text, which replaced
    /// invalid UTF-8 with U+FFFD, normalised CRLF to LF, dropped the BOM and
    /// stripped a trailing '\r' belonging to the data — while
    /// `recording::export`, the other half of the same "export filtered
    /// results" feature, deliberately wrote the original bytes. The lossy path
    /// was the large-file one, where the user is least able to re-obtain the
    /// source.
    #[test]
    fn export_preserves_the_original_bytes_of_every_matching_row() {
        let path = file(0);
        // BOM, then a CRLF row, an invalid-UTF-8 row, and a row whose data ends
        // in a real '\r' before the LF.
        let mut bytes = Vec::from(b"\xef\xbb\xbf");
        bytes.extend_from_slice(b"01-01 00:00:00.000  1  2 I Keep: crlf row\r\n");
        bytes.extend_from_slice(b"01-01 00:00:00.000  1  2 I Drop: skipped\n");
        bytes.extend_from_slice(b"01-01 00:00:00.000  1  2 I Keep: bad \xff\xfe byte\n");
        bytes.extend_from_slice(b"01-01 00:00:00.000  1  2 I Keep: trailing cr\r\r\n");
        std::fs::write(&path, &bytes).unwrap();

        let destination = path.with_extension("filtered");
        let spec = FilterSpec {
            allowed_tags: Some(["Keep".to_string()].into()),
            ..Default::default()
        };
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        assert_eq!(export_filtered(&index, &destination, &spec).unwrap(), 3);

        let out = std::fs::read(&destination).unwrap();
        let mut want = Vec::from(b"\xef\xbb\xbf");
        want.extend_from_slice(b"01-01 00:00:00.000  1  2 I Keep: crlf row\r\n");
        want.extend_from_slice(b"01-01 00:00:00.000  1  2 I Keep: bad \xff\xfe byte\n");
        want.extend_from_slice(b"01-01 00:00:00.000  1  2 I Keep: trailing cr\r\r\n");
        assert_eq!(
            out, want,
            "export must reproduce the source bytes of the matching rows"
        );

        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(destination).unwrap();
    }

    #[test]
    fn long_rows_after_the_sample_obey_budget_and_keep_every_source_line() {
        let path = file(0);
        let mut output = io::BufWriter::new(File::create(&path).unwrap());
        // The entire line-length sample contains short rows; later pages contain
        // long rows, so the page budget has to hold against rows it never sampled.
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
        let error = crate::io::read_line_bounded(
            &mut reader,
            &mut raw,
            MAX_LINE_BYTES,
            &|| cancel.load(Ordering::Acquire),
            line_too_long_message(MAX_LINE_BYTES),
        )
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
    fn export_replaces_an_existing_destination() {
        // The save dialog already asked the user to confirm the replacement; the
        // export must then actually replace it (and leave no temp file behind).
        let path = file(3);
        let index = build_index(&path, 6 * 1024 * 1024, egui::Context::default()).unwrap();
        wait(&index);
        let destination = path.with_extension("export");
        std::fs::write(&destination, b"stale content\n").unwrap();

        assert_eq!(
            export_filtered(&index, &destination, &FilterSpec::default()).unwrap(),
            3
        );
        let text = std::fs::read_to_string(&destination).unwrap();
        assert!(
            text.contains("message 0"),
            "the export must be written: {text:?}"
        );
        assert!(!text.contains("stale"), "the old content must be gone");
        let prefix = format!("{}.", destination.file_name().unwrap().to_string_lossy());
        let leftovers: Vec<String> = std::fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&prefix))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp export left behind: {leftovers:?}"
        );
        std::fs::remove_file(&destination).unwrap();
        std::fs::remove_file(path).unwrap();
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
