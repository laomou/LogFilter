//! Raw recording is independent of the preview. Only file offsets and small,
//! bounded preview batches cross threads; a slow UI leaves its backlog on disk.
use crate::filter::FilterSpec;
use crate::lock::MutexExt;
use crate::model::{LogEntry, LogFormat};
use crate::parser::parse_line_hinted;
use crossbeam_channel::{bounded, Receiver, SendTimeoutError};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BLOCK: usize = 64 * 1024;
const PREVIEW_LINE: usize = 1024 * 1024;
const BATCH_BYTES: usize = 256 * 1024;

/// Persistent output directory. No marker files or automatic deletion: users
/// can see and manage the original recordings directly in logs/.
pub struct RecordingDirectory {
    path: PathBuf,
}

impl RecordingDirectory {
    pub fn create(parent: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        Ok(Self {
            path: std::fs::canonicalize(parent)?,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn used_bytes(&self) -> io::Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(total)
    }
}

pub struct DiskBudget {
    pub limit: AtomicU64,
    pub min_free: AtomicU64,
    pub used: AtomicU64,
}

impl DiskBudget {
    pub fn new(limit: u64, min_free: u64) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicU64::new(limit),
            min_free: AtomicU64::new(min_free),
            used: AtomicU64::new(0),
        })
    }
    fn reserve(self: &Arc<Self>, bytes: u64) -> io::Result<Reservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.limit.load(Ordering::Relaxed))
            })
            .map_err(|_| {
                io::Error::other(
                    "logs directory size limit reached; capture stopped, original files retained",
                )
            })?;
        Ok(Reservation {
            budget: self.clone(),
            remaining: bytes,
        })
    }
    fn check_space(&self, available: u64) -> io::Result<()> {
        if available
            < self
                .min_free
                .load(Ordering::Relaxed)
                .saturating_add(BLOCK as u64)
        {
            Err(io::Error::other(
                "disk free-space safety margin reached; capture stopped",
            ))
        } else {
            Ok(())
        }
    }
}

struct Reservation {
    budget: Arc<DiskBudget>,
    remaining: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.remaining, Ordering::AcqRel);
    }
}

#[cfg(unix)]
fn available_space(file: &File, _path: &Path) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: file owns a valid fd, and stat points to writable statvfs storage.
    if unsafe { libc::fstatvfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatvfs succeeded and initialized the structure.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(windows)]
fn available_space(_file: &File, path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let directory: Vec<u16> = path
        .parent()
        .unwrap_or(path)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut free = 0;
    // SAFETY: directory is NUL-terminated and free is valid writable storage.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            directory.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(free)
    }
}

#[cfg(not(any(unix, windows)))]
fn available_space(_file: &File, _path: &Path) -> io::Result<u64> {
    Err(io::Error::other(
        "disk free-space checks are unavailable on this platform",
    ))
}

pub struct Recording {
    pub path: PathBuf,
    pub written: AtomicU64,
    pub finished: AtomicBool,
    pub paused: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
    pub error: Mutex<Option<String>>,
    pub preview_error: Mutex<Option<String>>,
    pub error_reported: AtomicBool,
    pub budget: Arc<DiskBudget>,
}

pub struct Writer {
    pub state: Arc<Recording>,
    file: File,
}

impl Writer {
    #[cfg(test)]
    pub fn create(directory: &Path) -> io::Result<Self> {
        Self::create_with_budget(directory, DiskBudget::new(u64::MAX, 0))
    }

    pub fn create_with_budget(directory: &Path, budget: Arc<DiskBudget>) -> io::Result<Self> {
        if budget.used.load(Ordering::Acquire) >= budget.limit.load(Ordering::Relaxed) {
            return Err(io::Error::other(
                "logs directory size limit reached; manage existing logs or raise the limit",
            ));
        }
        std::fs::create_dir_all(directory)?;
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        for sequence in 0..1000 {
            let name = if sequence == 0 {
                format!("logfilter_{stamp}.log")
            } else {
                format!("logfilter_{stamp}_{sequence}.log")
            };
            // Keep the active file visibly marked until close/final sync. Startup
            // counts these incomplete recordings against the directory limit.
            let path = directory.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    if budget.min_free.load(Ordering::Relaxed) > 0 {
                        if let Err(e) =
                            available_space(&file, &path).and_then(|n| budget.check_space(n))
                        {
                            drop(file);
                            let _ = std::fs::remove_file(&path);
                            return Err(e);
                        }
                    }
                    return Ok(Self {
                        state: Arc::new(Recording {
                            path,
                            written: AtomicU64::new(0),
                            finished: AtomicBool::new(false),
                            paused: Arc::new(AtomicBool::new(false)),
                            stop: Arc::new(AtomicBool::new(false)),
                            error: Mutex::new(None),
                            preview_error: Mutex::new(None),
                            error_reported: AtomicBool::new(false),
                            budget,
                        }),
                        file,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot create a unique recording file",
        ))
    }

    pub fn record(mut self, reader: impl Read) {
        let state = self.state.clone();
        let result = copy_raw(
            reader,
            &mut self.file,
            &state,
            |f| f.sync_data(),
            |f| available_space(f, &state.path),
        );
        if let Err(e) = result {
            *state.error.lock_recover() = Some(format!("{}: {e}", state.path.display()));
            state.stop.store(true, Ordering::Release);
        }
        state.finished.store(true, Ordering::Release);
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.state.finished.load(Ordering::Acquire) {
            let error = match self.file.sync_data() {
                Ok(()) => "recording worker did not finish".to_string(),
                Err(e) => e.to_string(),
            };
            *self.state.error.lock_recover() = Some(error);
            self.state.finished.store(true, Ordering::Release);
        }
    }
}

/// Write before publishing an offset; count partial writes even when a later
/// write fails. The file keeps original bytes, including invalid UTF-8 and CRLF.
fn copy_raw<R: Read, W: Write>(
    mut reader: R,
    writer: &mut W,
    state: &Recording,
    mut sync: impl FnMut(&mut W) -> io::Result<()>,
    mut free_space: impl FnMut(&mut W) -> io::Result<u64>,
) -> io::Result<()> {
    let mut buf = [0u8; BLOCK];
    let mut last_sync = Instant::now();
    let mut bytes_since_space_check = 1024 * 1024;
    let read_result = (|| {
        loop {
            if state.budget.min_free.load(Ordering::Relaxed) > 0
                && (bytes_since_space_check >= 1024 * 1024
                    || last_sync.elapsed() >= Duration::from_secs(1))
            {
                state.budget.check_space(free_space(writer)?)?;
                bytes_since_space_check = 0;
            }
            let n = match reader.read(&mut buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                break;
            }
            let mut reservation = state.budget.reserve(n as u64)?;
            let mut offset = 0;
            while offset < n {
                match writer.write(&buf[offset..n]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "recording write returned zero",
                        ))
                    }
                    Ok(count) => {
                        offset += count;
                        state.written.fetch_add(count as u64, Ordering::Release);
                        reservation.remaining -= count as u64;
                        bytes_since_space_check += count;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            if last_sync.elapsed() >= Duration::from_secs(1) {
                writer.flush()?;
                sync(writer)?;
                last_sync = Instant::now();
            }
            if state.stop.load(Ordering::Acquire) {
                break;
            }
        }
        Ok(())
    })();
    // Final sync is attempted even after a read/write failure so the captured
    // prefix can be retained. A failure here must never be reported as success.
    let finish_result = writer.flush().and_then(|_| sync(writer));
    read_result.and(finish_result)
}

pub struct PreviewBatch {
    pub epoch: u64,
    pub entries: Vec<LogEntry>,
    pub offset: u64,
    pub omitted: u64,
}

/// A single-slot channel: at most one queued batch and one being assembled.
/// No amount of disk backlog can increase its resident memory.
pub fn preview(
    state: Arc<Recording>,
    start: u64,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    ctx: egui::Context,
) -> io::Result<Receiver<PreviewBatch>> {
    let mut file = File::open(&state.path)?;
    let mut skip_partial = false;
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        skip_partial = previous[0] != b'\n';
    }
    file.seek(SeekFrom::Start(start))?;
    let (tx, rx) = bounded(1);
    std::thread::Builder::new()
        .name("recording-preview".into())
        .spawn(move || {
            let result = (|| -> io::Result<()> {
                let mut offset = start;
                let mut raw = [0u8; BLOCK];
                let mut line = Vec::new();
                let mut skipping = skip_partial;
                let mut hint = LogFormat::Unknown;
                let mut sent_offset = start;
                loop {
                    if source_epoch.load(Ordering::Acquire) != epoch {
                        return Ok(());
                    }
                    if state.paused.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let done = state.finished.load(Ordering::Acquire);
                    let end = state.written.load(Ordering::Acquire);
                    let mut entries = Vec::new();
                    let mut bytes = 0;
                    let mut omitted = 0;
                    let mut scanned = 0;
                    while offset < end
                        && bytes < BATCH_BYTES
                        && entries.len() < 512
                        && scanned < BATCH_BYTES
                    {
                        if source_epoch.load(Ordering::Acquire) != epoch {
                            return Ok(());
                        }
                        let want = (end - offset).min(raw.len() as u64) as usize;
                        let n = file.read(&mut raw[..want])?;
                        if n == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "recording file was truncated",
                            ));
                        }
                        let mut consumed = 0;
                        for chunk in raw[..n].split_inclusive(|&b| b == b'\n') {
                            consumed += chunk.len();
                            offset += chunk.len() as u64;
                            let newline = chunk.last() == Some(&b'\n');
                            if !skipping {
                                if line.len() + chunk.len() > PREVIEW_LINE {
                                    line.clear();
                                    skipping = true;
                                    omitted += 1;
                                } else {
                                    line.try_reserve(chunk.len()).map_err(|_| {
                                        io::Error::new(
                                            io::ErrorKind::OutOfMemory,
                                            "preview buffer allocation failed",
                                        )
                                    })?;
                                    line.extend_from_slice(chunk);
                                }
                            }
                            if newline {
                                if !skipping {
                                    push_preview(&mut entries, &mut bytes, &mut line, &mut hint);
                                }
                                skipping = false;
                            }
                            if bytes >= BATCH_BYTES || entries.len() >= 512 {
                                break;
                            }
                        }
                        scanned += consumed;
                        if consumed < n {
                            file.seek(SeekFrom::Start(offset))?;
                        }
                    }
                    if done && offset == end && !line.is_empty() && !skipping {
                        push_preview(&mut entries, &mut bytes, &mut line, &mut hint);
                    }
                    if !done && offset == sent_offset && entries.is_empty() && omitted == 0 {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let mut batch = PreviewBatch {
                        epoch,
                        entries,
                        offset,
                        omitted,
                    };
                    loop {
                        if source_epoch.load(Ordering::Acquire) != epoch {
                            return Ok(());
                        }
                        match tx.send_timeout(batch, Duration::from_millis(50)) {
                            Ok(()) => {
                                sent_offset = offset;
                                ctx.request_repaint();
                                break;
                            }
                            Err(SendTimeoutError::Timeout(pending)) => batch = pending,
                            Err(SendTimeoutError::Disconnected(_)) => return Ok(()),
                        }
                    }
                    if done && offset == end {
                        return Ok(());
                    }
                    if offset == end {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            })();
            if let Err(e) = result {
                *state.preview_error.lock_recover() = Some(e.to_string());
                ctx.request_repaint();
            }
        })?;
    Ok(rx)
}

fn push_preview(
    entries: &mut Vec<LogEntry>,
    bytes: &mut usize,
    line: &mut Vec<u8>,
    hint: &mut LogFormat,
) {
    let text = String::from_utf8_lossy(line)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    let (entry, format) = parse_line_hinted(text, *hint);
    if *hint == LogFormat::Unknown {
        *hint = format;
    }
    *bytes += entry.estimated_bytes() as usize;
    entries.push(entry);
    line.clear();
}

/// Export only the byte range already written when the user requested it. The
/// source file can continue growing, and filter changes cannot alter this job.
pub fn export(
    source: &Path,
    length: u64,
    destination: &Path,
    filter: &FilterSpec,
) -> io::Result<u64> {
    if filter.bookmarks_only {
        return Err(io::Error::other(
            "bookmark export is available for the cached view only",
        ));
    }
    let input = File::open(source)?;
    if let Ok(existing) = File::open(destination) {
        if crate::io::same_file(&input, &existing)? {
            return Err(io::Error::other("cannot overwrite the raw recording"));
        }
    }
    if input.metadata()?.len() < length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "recording was truncated before export",
        ));
    }
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut writer = io::BufWriter::new(output);
    let mut reader = input.take(length);
    let bookmarks = std::collections::HashSet::new();
    let mut rows = 0u64;
    let mut hint = LogFormat::Unknown;
    crate::io::visit_utf8_lines(&mut reader, |line| {
        let (entry, format) = parse_line_hinted(line, hint);
        if hint == LogFormat::Unknown {
            hint = format;
        }
        if filter.matches(&entry, 0, &bookmarks) {
            writer.write_all(entry.raw().as_bytes())?;
            writer.write_all(b"\n")?;
            rows += 1;
        }
        Ok(())
    })?;
    if reader.limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "recording was truncated during export",
        ));
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn writer() -> Writer {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lf_disk_recording_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Writer::create(&dir).unwrap()
    }

    fn cleanup(state: &Recording) {
        std::fs::remove_file(&state.path).unwrap();
        std::fs::remove_dir(state.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn recording_names_are_readable_unique_and_persistent() {
        let first = writer();
        let directory = first.state.path.parent().unwrap().to_owned();
        let second = Writer::create(&directory).unwrap();
        let first_name = first.state.path.file_name().unwrap().to_string_lossy();
        let second_name = second.state.path.file_name().unwrap().to_string_lossy();
        assert!(first_name.starts_with("logfilter_20") && first_name.ends_with(".log"));
        assert_ne!(first_name, second_name);
        let first_state = first.state.clone();
        let second_state = second.state.clone();
        first.record(Cursor::new(b"one\n"));
        second.record(Cursor::new(b"two\n"));
        assert_eq!(
            RecordingDirectory::create(&directory)
                .unwrap()
                .used_bytes()
                .unwrap(),
            8
        );
        let first_path = first_state.path.clone();
        drop(first_state);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"one\n");
        std::fs::remove_file(&first_path).unwrap();
        std::fs::remove_file(&second_state.path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn directory_limit_stops_capture_and_retains_the_written_prefix() {
        let writer = writer();
        let state = writer.state.clone();
        state.budget.limit.store(70 * 1024, Ordering::Relaxed);
        writer.record(io::repeat(b'x').take(100 * 1024));
        assert_eq!(state.written.load(Ordering::Acquire), BLOCK as u64);
        assert_eq!(std::fs::metadata(&state.path).unwrap().len(), BLOCK as u64);
        assert!(state
            .error
            .lock_recover()
            .as_ref()
            .unwrap()
            .contains("size limit"));
        cleanup(&state);
    }

    #[test]
    fn free_space_guard_has_a_safety_margin() {
        let budget = DiskBudget::new(u64::MAX, 1024 * 1024);
        assert!(budget.check_space(1024 * 1024).is_err());
        assert!(budget.check_space(1024 * 1024 + BLOCK as u64 - 1).is_err());
        assert!(budget.check_space(1024 * 1024 + BLOCK as u64).is_ok());
    }

    #[test]
    fn raw_bytes_are_saved_verbatim_even_while_preview_is_paused() {
        let writer = writer();
        let state = writer.state.clone();
        state.paused.store(true, Ordering::Relaxed);
        let bytes = b"\xef\xbb\xbffirst\r\n\xff\x00\nunterminated";
        writer.record(Cursor::new(bytes));
        assert_eq!(std::fs::read(&state.path).unwrap(), bytes);
        assert_eq!(state.written.load(Ordering::Acquire), bytes.len() as u64);
        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_none());
        cleanup(&state);
    }

    #[test]
    fn blocked_preview_cannot_block_or_buffer_the_recording() {
        let writer = writer();
        let state = writer.state.clone();
        let epoch = Arc::new(AtomicU64::new(1));
        let rx = preview(state.clone(), 0, 1, epoch.clone(), egui::Context::default()).unwrap();
        let (done_tx, done_rx) = bounded(1);
        let worker = std::thread::spawn(move || {
            writer.record(io::repeat(b'\n').take(2 * 1024 * 1024));
            done_tx.send(()).unwrap();
        });
        // Never consume the preview. A full queue or slow model must not stop
        // raw recording, even though this file represents two million rows.
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert_eq!(state.written.load(Ordering::Acquire), 2 * 1024 * 1024);
        assert!(state.error.lock_recover().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while rx.is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(rx.len(), 1);
        let batch = rx.recv().unwrap();
        assert!(batch.entries.len() <= 512);
        assert!(
            batch
                .entries
                .iter()
                .map(LogEntry::estimated_bytes)
                .sum::<u64>()
                <= BATCH_BYTES as u64
        );
        epoch.store(2, Ordering::Release);
        drop(rx);
        cleanup(&state);
    }

    #[test]
    fn preview_catches_up_in_order_after_pause_and_keeps_final_partial_line() {
        let writer = writer();
        let state = writer.state.clone();
        state.paused.store(true, Ordering::Relaxed);
        let epoch = Arc::new(AtomicU64::new(1));
        let rx = preview(state.clone(), 0, 1, epoch, egui::Context::default()).unwrap();
        let text = (0..3000)
            .map(|i| format!("line {i}\r\n"))
            .collect::<String>()
            + "final";
        writer.record(Cursor::new(text.as_bytes()));
        assert!(rx.is_empty());
        state.paused.store(false, Ordering::Relaxed);
        let mut got = Vec::new();
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(5)) {
            assert!(batch.entries.len() <= 512);
            got.extend(batch.entries.into_iter().map(|e| e.raw().to_owned()));
        }
        assert_eq!(got.len(), 3001);
        for (i, line) in got[..3000].iter().enumerate() {
            assert_eq!(line, &format!("line {i}"));
        }
        assert_eq!(got.last().unwrap(), "final");
        cleanup(&state);
    }

    #[test]
    fn oversized_preview_line_is_still_recorded_completely() {
        let writer = writer();
        let state = writer.state.clone();
        let source = io::repeat(0xff)
            .take((PREVIEW_LINE * 3) as u64)
            .chain(Cursor::new(b"\nafter\n"));
        writer.record(source);
        assert_eq!(
            std::fs::metadata(&state.path).unwrap().len(),
            (PREVIEW_LINE * 3 + 7) as u64
        );
        let rx = preview(
            state.clone(),
            0,
            1,
            Arc::new(AtomicU64::new(1)),
            egui::Context::default(),
        )
        .unwrap();
        let mut omitted = 0;
        let mut got = Vec::new();
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(5)) {
            omitted += batch.omitted;
            got.extend(batch.entries.into_iter().map(|e| e.raw().to_owned()));
        }
        assert_eq!(omitted, 1);
        assert_eq!(got, ["after"]);
        cleanup(&state);
    }

    #[test]
    fn clear_boundary_does_not_replay_an_old_partial_line() {
        let writer = writer();
        let state = writer.state.clone();
        writer.record(Cursor::new(b"old partial\nnew\n"));
        let rx = preview(
            state.clone(),
            4,
            2,
            Arc::new(AtomicU64::new(2)),
            egui::Context::default(),
        )
        .unwrap();
        let mut got = Vec::new();
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(5)) {
            got.extend(batch.entries.into_iter().map(|e| e.raw().to_owned()));
        }
        assert_eq!(got, ["new"]);
        assert_eq!(std::fs::read(&state.path).unwrap(), b"old partial\nnew\n");
        cleanup(&state);
    }

    struct FailingWriter {
        bytes: Vec<u8>,
        remaining: usize,
        fail_flush: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("disk full"));
            }
            let n = bytes.len().min(self.remaining).min(3);
            self.bytes.extend_from_slice(&bytes[..n]);
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn write_flush_and_sync_errors_are_not_reported_as_success() {
        for failure in 0..3 {
            let writer = writer();
            let state = writer.state.clone();
            let mut dest = FailingWriter {
                bytes: vec![],
                remaining: if failure == 0 { 5 } else { 100 },
                fail_flush: failure == 1,
            };
            let error = copy_raw(
                Cursor::new(b"0123456789"),
                &mut dest,
                &state,
                |_| {
                    if failure == 2 {
                        Err(io::Error::other("sync failed"))
                    } else {
                        Ok(())
                    }
                },
                |_| Ok(u64::MAX),
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains(["disk full", "flush failed", "sync failed"][failure]));
            assert_eq!(
                state.written.load(Ordering::Acquire),
                dest.bytes.len() as u64
            );
            assert_eq!(dest.bytes, b"0123456789"[..dest.bytes.len()]);
            drop(writer);
            cleanup(&state);
        }
    }

    #[test]
    fn read_error_retains_and_syncs_the_captured_prefix() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("device read failed"))
            }
        }
        let writer = writer();
        let state = writer.state.clone();
        writer.record(Cursor::new(b"prefix\n").chain(Broken));
        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.stop.load(Ordering::Acquire));
        assert!(state
            .error
            .lock_recover()
            .as_ref()
            .unwrap()
            .contains("device read failed"));
        assert_eq!(std::fs::read(&state.path).unwrap(), b"prefix\n");
        cleanup(&state);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disk_full_stops_recording_and_marks_the_file_incomplete() {
        let mut writer = writer();
        let state = writer.state.clone();
        writer.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
        writer.record(Cursor::new(b"cannot fit\n"));
        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.stop.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_some());
        assert_eq!(state.written.load(Ordering::Acquire), 0);
        cleanup(&state);
    }

    #[test]
    fn filtered_export_uses_the_requested_disk_snapshot() {
        let writer = writer();
        let state = writer.state.clone();
        let raw = b"I/Tag( 1): keep first\r\nE/Tag( 1): remove second\r\n";
        writer.record(Cursor::new(raw));
        // Simulate recording continuing after the export snapshot was selected.
        OpenOptions::new()
            .append(true)
            .open(&state.path)
            .unwrap()
            .write_all(b"I/Tag( 1): keep later\n")
            .unwrap();
        let filtered = state.path.with_extension("filtered");
        let spec = FilterSpec {
            find: vec!["keep".into()],
            ..Default::default()
        };
        assert_eq!(
            export(&state.path, raw.len() as u64, &filtered, &spec).unwrap(),
            1
        );
        assert_eq!(
            std::fs::read(&filtered).unwrap(),
            b"I/Tag( 1): keep first\n"
        );
        std::fs::remove_file(filtered).unwrap();
        cleanup(&state);
    }

    #[test]
    fn export_rejects_overwriting_the_recording_and_truncated_snapshots() {
        let writer = writer();
        let state = writer.state.clone();
        writer.record(Cursor::new(b"original\n"));
        let spec = FilterSpec::default();
        assert!(export(&state.path, 9, &state.path, &spec).is_err());
        assert_eq!(std::fs::read(&state.path).unwrap(), b"original\n");
        let output = state.path.with_extension("export");
        assert!(export(&state.path, 100, &output, &spec).is_err());
        assert!(!output.exists());
        #[cfg(unix)]
        {
            std::fs::hard_link(&state.path, &output).unwrap();
            assert!(export(&state.path, 9, &output, &spec).is_err());
            assert_eq!(std::fs::read(&state.path).unwrap(), b"original\n");
            std::fs::remove_file(output).unwrap();
        }
        cleanup(&state);
    }
}
