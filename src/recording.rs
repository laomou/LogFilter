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

/// How many bytes may be written between two free-space probes.
///
/// `check_space` reserves this much on top of the configured minimum, because
/// a probe that passes is a promise about the *next* interval, not just the
/// current instant. The two must move together: with the margin smaller than
/// the interval, a probe passing at exactly the limit still allowed a further
/// interval's worth of writes, deterministically eating into the reserve the
/// user configured.
const SPACE_CHECK_BYTES: u64 = 1024 * 1024;
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
}

pub struct DiskBudget {
    pub min_free: AtomicU64,
}

impl DiskBudget {
    pub fn new(min_free: u64) -> Arc<Self> {
        Arc::new(Self {
            min_free: AtomicU64::new(min_free),
        })
    }
    /// Whether a capture may keep writing for one more probe interval.
    ///
    /// The margin covers everything writable before the next probe: a full
    /// interval plus the one block that can carry the counter past it.
    fn check_space(&self, available: u64) -> io::Result<()> {
        if available
            < self
                .min_free
                .load(Ordering::Relaxed)
                .saturating_add(SPACE_CHECK_BYTES)
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
        Self::create_with_budget(directory, DiskBudget::new(0))
    }

    pub fn create_with_budget(directory: &Path, budget: Arc<DiskBudget>) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        for sequence in 0..1000 {
            let name = if sequence == 0 {
                format!("logfilter_{stamp}.log")
            } else {
                format!("logfilter_{stamp}_{sequence}.log")
            };
            // Names remain persistent after Stop; never overwrite older captures.
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
    // Its own clock: `last_sync` is only reset by a successful sync, so sharing
    // it made "probe at least once a second" mean "a second since the last
    // successful sync" — which is a different thing whenever syncing is slow or
    // failing, exactly when free space is worth watching.
    let mut last_space_check = Instant::now();
    let mut bytes_since_space_check = SPACE_CHECK_BYTES as usize;
    // Reads may split a log line anywhere. Track both the discarded stream's
    // boundary and the saved prefix without buffering arbitrarily long lines.
    let mut skip_partial = false;
    let mut saved_line_open = false;
    let mut paused_bytes = false;
    let read_result = (|| {
        loop {
            if state.stop.load(Ordering::Acquire) {
                break;
            }
            // A paused capture writes nothing, so a free-space failure it would
            // report can only come from other writers filling the disk; do not
            // stop the recording over bytes it never needed to write.
            if state.budget.min_free.load(Ordering::Relaxed) > 0
                && !state.paused.load(Ordering::Acquire)
                && (bytes_since_space_check >= SPACE_CHECK_BYTES as usize
                    || last_space_check.elapsed() >= Duration::from_secs(1))
            {
                state.budget.check_space(free_space(writer)?)?;
                bytes_since_space_check = 0;
                last_space_check = Instant::now();
            }
            let n = match reader.read(&mut buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            // Stop wins over pause: checking it only after the write path would
            // make a paused capture that never reaches EOF (a descendant still
            // holding the pipe) discard data forever, never finish, and leave the
            // window unable to close.
            if state.stop.load(Ordering::Acquire) {
                break;
            }
            if n == 0 {
                break;
            }
            if state.paused.load(Ordering::Acquire) {
                paused_bytes = true;
                skip_partial = buf[n - 1] != b'\n';
                continue;
            }
            let start = if skip_partial {
                let Some(newline) = buf[..n].iter().position(|&b| b == b'\n') else {
                    continue;
                };
                skip_partial = false;
                newline + 1
            } else {
                0
            };
            if start == n {
                continue;
            }
            // A partial prefix was already published to the preview, so it
            // cannot be retracted safely. Terminate it before saving a new line;
            // do not insert anything if pause discarded no bytes or at EOF.
            let separator: &[u8] = if paused_bytes && saved_line_open {
                b"\n"
            } else {
                b""
            };
            for bytes in [separator, &buf[start..n]] {
                let mut offset = 0;
                while offset < bytes.len() {
                    match writer.write(&bytes[offset..]) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "recording write returned zero",
                            ))
                        }
                        Ok(count) => {
                            offset += count;
                            state.written.fetch_add(count as u64, Ordering::Release);
                            bytes_since_space_check += count;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            saved_line_open = buf[n - 1] != b'\n';
            paused_bytes = false;
            if last_sync.elapsed() >= Duration::from_secs(1) {
                writer.flush()?;
                sync(writer)?;
                last_sync = Instant::now();
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
pub struct PreparedPreview {
    activate: crossbeam_channel::Sender<(u64, Arc<AtomicU64>)>,
    batches: Receiver<PreviewBatch>,
}

impl PreparedPreview {
    pub fn activate(self, epoch: u64, source_epoch: Arc<AtomicU64>) -> Receiver<PreviewBatch> {
        // The worker waits for this handover; dropping an unactivated preview
        // disconnects the channel and exits without touching any source.
        let _ = self.activate.send((epoch, source_epoch));
        self.batches
    }
}

pub fn preview(
    state: Arc<Recording>,
    start: u64,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    ctx: egui::Context,
) -> io::Result<Receiver<PreviewBatch>> {
    Ok(prepare_preview(state, start, ctx)?.activate(epoch, source_epoch))
}

pub fn prepare_preview(
    state: Arc<Recording>,
    start: u64,
    ctx: egui::Context,
) -> io::Result<PreparedPreview> {
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
    let (activate, activated) = bounded::<(u64, Arc<AtomicU64>)>(1);
    std::thread::Builder::new()
        .name("recording-preview".into())
        .spawn(move || {
            let Ok((epoch, source_epoch)) = activated.recv() else {
                return;
            };
            let result = (|| -> io::Result<()> {
                let mut offset = start;
                let mut raw = [0u8; BLOCK];
                let mut line = Vec::new();
                let mut skipping = skip_partial;
                let mut hint = LogFormat::Unknown;
                let mut sent_offset = start;
                // The BOM (when the device emitted one) sits at byte 0 of the
                // file: it belongs to the first row, and only the first row. It
                // would otherwise break the timestamp parse of row 1.
                let mut first_line = start == 0;
                loop {
                    if source_epoch.load(Ordering::Acquire) != epoch {
                        return Ok(());
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
                                    // The over-long row is discarded: if it was
                                    // the file's first row, its BOM went with it.
                                    first_line = false;
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
                                    let strip = first_line;
                                    first_line = false;
                                    push_preview(
                                        &mut entries,
                                        &mut bytes,
                                        &mut line,
                                        &mut hint,
                                        strip,
                                    );
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
                        let strip = first_line;
                        first_line = false;
                        push_preview(&mut entries, &mut bytes, &mut line, &mut hint, strip);
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
    Ok(PreparedPreview {
        activate,
        batches: rx,
    })
}

fn push_preview(
    entries: &mut Vec<LogEntry>,
    bytes: &mut usize,
    line: &mut Vec<u8>,
    hint: &mut LogFormat,
    strip_bom: bool,
) {
    let cow = String::from_utf8_lossy(line);
    let text = if strip_bom {
        cow.trim_start_matches('\u{feff}')
    } else {
        cow.as_ref()
    };
    let text = text.trim_end_matches(['\r', '\n']).to_string();
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
    // A confirmed overwrite is published only after the snapshot is fully
    // written and synced; failures leave any existing export untouched.
    let tmp = crate::config::temp_sibling(destination);
    let output = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let mut writer = io::BufWriter::new(output);
    let mut reader = io::BufReader::new(input.take(length));
    let bookmarks = std::collections::HashSet::new();
    let mut rows = 0u64;
    let mut hint = LogFormat::Unknown;
    let mut raw = Vec::new();
    // A BOM at byte 0 belongs to row 1's *text* (it would break the timestamp
    // parse) but the raw export bytes are copied verbatim, BOM included.
    let mut first_line = true;
    let body: io::Result<()> = (|| {
        loop {
            let n = crate::io::read_line_bounded(
                &mut reader,
                &mut raw,
                crate::io::MAX_INITIAL_LINE_BYTES,
                &|| false,
                "log line exceeds the 64 MiB export limit",
            )?;
            if n == 0 {
                break;
            }
            // Match on the decoded text but write the *original bytes*: a raw
            // recording deliberately keeps invalid UTF-8, CRLF line endings and
            // any BOM, and a filtered export is a copy of the matching rows.
            // Decoding the output would turn invalid bytes into U+FFFD and
            // normalise line endings.
            let text = String::from_utf8_lossy(&raw);
            let text = if first_line {
                text.trim_start_matches('\u{feff}')
            } else {
                text.as_ref()
            };
            first_line = false;
            let (entry, format) =
                parse_line_hinted(text.trim_end_matches(['\r', '\n']).to_string(), hint);
            if hint == LogFormat::Unknown {
                hint = format;
            }
            if filter.matches(&entry, 0, &bookmarks) {
                writer.write_all(&raw)?;
                rows += 1;
            }
        }
        if reader.get_ref().limit() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "recording was truncated during export",
            ));
        }
        writer.flush()?;
        writer.get_ref().sync_all()
    })();
    drop(writer);
    if let Err(e) = body.and_then(|()| std::fs::rename(&tmp, destination)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    crate::config::sync_parent_dir(destination);
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
        assert_eq!(std::fs::metadata(&first_state.path).unwrap().len(), 4);
        assert_eq!(std::fs::metadata(&second_state.path).unwrap().len(), 4);
        let first_path = first_state.path.clone();
        drop(first_state);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"one\n");
        std::fs::remove_file(&first_path).unwrap();
        std::fs::remove_file(&second_state.path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn free_space_guard_has_a_safety_margin() {
        let min_free = 1024 * 1024;
        let budget = DiskBudget::new(min_free);
        // The margin must cover a whole probe interval, not just one block:
        // after a probe passes, the loop may write SPACE_CHECK_BYTES plus the
        // block that carries the counter past the threshold before probing
        // again, and all of that has to fit above `min_free`.
        let margin = SPACE_CHECK_BYTES + BLOCK as u64;
        assert!(budget.check_space(min_free).is_err());
        assert!(budget.check_space(min_free + margin - 1).is_err());
        assert!(budget.check_space(min_free + margin).is_ok());
        assert!(
            margin >= SPACE_CHECK_BYTES,
            "a probe promises the next interval, so the margin cannot be smaller than it"
        );
    }

    #[test]
    fn paused_recording_discards_bytes_without_writing() {
        let writer = writer();
        let state = writer.state.clone();
        state.paused.store(true, Ordering::Relaxed);
        let bytes = b"\xef\xbb\xbffirst\r\n\xff\x00\nunterminated";
        writer.record(Cursor::new(bytes));
        assert!(std::fs::read(&state.path).unwrap().is_empty());
        assert_eq!(state.written.load(Ordering::Acquire), 0);
        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_none());
        cleanup(&state);
    }

    struct PauseReader<'a> {
        state: Arc<Recording>,
        steps: &'a [(bool, &'a [u8])],
        step: usize,
    }

    impl Read for PauseReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(&(paused, bytes)) = self.steps.get(self.step) else {
                return Ok(0);
            };
            self.step += 1;
            assert!(bytes.len() <= buf.len());
            self.state.paused.store(paused, Ordering::Release);
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    fn assert_pause_output(steps: &[(bool, &[u8])], expected: &[u8]) {
        let writer = writer();
        let state = writer.state.clone();
        writer.record(PauseReader {
            state: state.clone(),
            steps,
            step: 0,
        });
        assert_eq!(std::fs::read(&state.path).unwrap(), expected);
        assert_eq!(state.written.load(Ordering::Acquire), expected.len() as u64);
        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_none());
        cleanup(&state);
    }

    #[test]
    fn pause_does_not_join_a_saved_prefix_to_the_next_log_line() {
        assert_pause_output(
            &[
                (false, b"before"),
                (true, b" paused\n"),
                (false, b"after\n"),
            ],
            b"before\nafter\n",
        );
    }

    #[test]
    fn resume_skips_a_paused_line_across_multiple_reads() {
        assert_pause_output(
            &[
                (false, b"before"),
                (true, b" discarded\npaused"),
                (false, b" line"),
                (false, b" tail\n"),
                (false, b"after\n"),
            ],
            b"before\nafter\n",
        );
    }

    #[test]
    fn pause_at_line_boundaries_preserves_bytes_without_extra_newlines() {
        assert_pause_output(
            &[
                (false, b"\xef\xbb\xbffirst\r\n"),
                (true, b"discarded\r\n"),
                (false, b"\xff\x00\r\nlast"),
            ],
            b"\xef\xbb\xbffirst\r\n\xff\x00\r\nlast",
        );
    }

    #[test]
    fn resume_skips_a_partial_line_even_when_nothing_was_saved_before_pause() {
        assert_pause_output(&[(true, b"paused"), (false, b" tail\nnext\n")], b"next\n");
    }

    #[test]
    fn repeated_pauses_keep_saved_prefixes_separate() {
        assert_pause_output(
            &[
                (false, b"first"),
                (true, b" tail\n"),
                (false, b"second"),
                (true, b" tail\nthird"),
                (false, b" tail\nfourth\n"),
            ],
            b"first\nsecond\nfourth\n",
        );
    }

    #[test]
    fn pause_followed_by_eof_does_not_modify_the_saved_prefix() {
        assert_pause_output(&[(false, b"before"), (true, b" tail\n")], b"before");
        assert_pause_output(
            &[(false, b"before"), (true, b" tail"), (false, b"more")],
            b"before",
        );
    }

    #[test]
    fn unpaused_split_lines_are_recorded_verbatim() {
        assert_pause_output(
            &[
                (false, b"first"),
                (false, b" tail\nsecond"),
                (false, b" tail\n"),
            ],
            b"first tail\nsecond tail\n",
        );
    }

    /// A reader that hands out `remaining` newline bytes and counts the reads it
    /// served, so a test can tell "stopped early" from "drained the whole pipe".
    struct CountingReader {
        remaining: usize,
        reads: usize,
    }

    impl std::io::Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            self.reads += 1;
            let n = buf.len().min(self.remaining);
            buf[..n].fill(b'\n');
            self.remaining -= n;
            Ok(n)
        }
    }

    #[test]
    fn stop_ends_a_paused_recording_without_draining_the_pipe() {
        // A paused capture used to check `stop` only at the end of the write path,
        // so with a descendant still holding stdout (no EOF) it escaped the loop
        // never, discarded data forever and kept `finished` unset — which is also
        // what wedges the window's close flow.
        let writer = writer();
        let state = writer.state.clone();
        state.paused.store(true, Ordering::Relaxed);
        state.stop.store(true, Ordering::Release);
        let mut reader = CountingReader {
            remaining: 64 * BLOCK,
            reads: 0,
        };

        writer.record(&mut reader);

        assert!(state.finished.load(Ordering::Acquire));
        assert_eq!(state.written.load(Ordering::Acquire), 0);
        assert!(
            reader.reads <= 1,
            "stop must be observed before the pipe is drained ({} blocks read)",
            reader.reads
        );
        cleanup(&state);
    }

    /// Delivers `data` in whole, then — on the *next* read, after the data
    /// has been written — arms `stop` and keeps serving filler bytes forever,
    /// so the capture can only end through the stop check. Arming on the read
    /// that returns the last data byte would drop that chunk: `copy_raw`
    /// checks stop before writing, so stop must be observed one iteration
    /// later, with the pipe still live (a device would never return EOF while
    /// it holds the write end).
    struct StopArmingReader {
        state: Arc<Recording>,
        data: Vec<u8>,
        offset: usize,
        stop_armed: bool,
    }

    impl Read for StopArmingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.stop_armed || self.offset >= self.data.len() {
                self.state.stop.store(true, Ordering::Release);
                self.stop_armed = true;
                buf[..1].fill(b'x');
                return Ok(1);
            }
            let n = buf.len().min(self.data.len() - self.offset);
            buf[..n].copy_from_slice(&self.data[self.offset..self.offset + n]);
            self.offset += n;
            Ok(n)
        }
    }

    #[test]
    fn stop_leaves_a_consistent_prefix_and_unterminated_tail() {
        // Stopping a live capture cuts the file mid-line. Everything written
        // before the cut must be intact and accounted for: the on-disk bytes,
        // `written`, the export and the preview must all agree on the
        // unterminated tail.
        let writer = writer();
        let state = writer.state.clone();
        let data = b"line one\nline two\npartial tail";
        writer.record(StopArmingReader {
            state: state.clone(),
            data: data.to_vec(),
            offset: 0,
            stop_armed: false,
        });

        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_none());
        assert_eq!(state.written.load(Ordering::Acquire), data.len() as u64);
        assert_eq!(std::fs::read(&state.path).unwrap(), data);

        // Export of the written snapshot keeps the partial tail byte for byte.
        let filtered = state.path.with_extension("filtered");
        let spec = FilterSpec {
            find: vec!["partial".into()],
            ..Default::default()
        };
        assert_eq!(
            export(&state.path, data.len() as u64, &filtered, &spec).unwrap(),
            1
        );
        assert_eq!(
            std::fs::read(&filtered).unwrap(),
            b"partial tail",
            "the unterminated tail must survive the export"
        );
        std::fs::remove_file(filtered).unwrap();

        // The preview of a finished recording ends with the same partial line.
        let rx = preview(
            state.clone(),
            0,
            1,
            Arc::new(AtomicU64::new(1)),
            egui::Context::default(),
        )
        .unwrap();
        let mut got = Vec::new();
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(5)) {
            got.extend(batch.entries.into_iter().map(|e| e.raw().to_owned()));
        }
        assert_eq!(got, ["line one", "line two", "partial tail"]);
        cleanup(&state);
    }

    #[test]
    fn stop_with_an_oversized_final_line() {
        // A final line longer than both the write block and the preview line
        // cap, terminated by stop rather than a newline: the writer must keep
        // every byte, the export must keep the whole line, and the preview
        // must omit it as oversized instead of buffering it without bound.
        let writer = writer();
        let state = writer.state.clone();
        let data = vec![b'A'; 2 * 1024 * 1024];
        writer.record(StopArmingReader {
            state: state.clone(),
            data: data.clone(),
            offset: 0,
            stop_armed: false,
        });

        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_none());
        assert_eq!(state.written.load(Ordering::Acquire), data.len() as u64);
        assert_eq!(std::fs::read(&state.path).unwrap(), data);

        let filtered = state.path.with_extension("filtered");
        assert_eq!(
            export(
                &state.path,
                data.len() as u64,
                &filtered,
                &FilterSpec::default()
            )
            .unwrap(),
            1
        );
        assert_eq!(
            std::fs::read(&filtered).unwrap(),
            data,
            "a line spanning many write blocks must survive stop + export"
        );
        std::fs::remove_file(filtered).unwrap();

        let rx = preview(
            state.clone(),
            0,
            1,
            Arc::new(AtomicU64::new(1)),
            egui::Context::default(),
        )
        .unwrap();
        let mut entries = 0;
        let mut omitted = 0;
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(5)) {
            entries += batch.entries.len();
            omitted += batch.omitted;
        }
        assert_eq!(entries, 0);
        assert_eq!(
            omitted, 1,
            "the oversized line is dropped from the preview, counted"
        );
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
    fn preview_catches_up_in_order_and_keeps_final_partial_line() {
        let writer = writer();
        let state = writer.state.clone();
        let epoch = Arc::new(AtomicU64::new(1));
        let rx = preview(state.clone(), 0, 1, epoch, egui::Context::default()).unwrap();
        let text = (0..3000)
            .map(|i| format!("line {i}\r\n"))
            .collect::<String>()
            + "final";
        writer.record(Cursor::new(text.as_bytes()));
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
    fn preview_strips_bom_from_first_row_only() {
        let writer = writer();
        let state = writer.state.clone();
        let epoch = Arc::new(AtomicU64::new(1));
        let rx = preview(state.clone(), 0, 1, epoch, egui::Context::default()).unwrap();
        let text = [
            &[0xEF, 0xBB, 0xBF][..],
            b"01-01 12:00:00.000 1 2 I 00400/Tag: first\n",
            b"01-01 12:00:00.001 1 2 I 00400/Tag: second\n",
        ]
        .concat();
        writer.record(Cursor::new(text));
        let mut got = Vec::new();
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(5)) {
            got.extend(batch.entries);
        }
        assert_eq!(got.len(), 2);
        // Row 1 must parse as a hilog row despite the BOM at byte 0.
        assert_eq!(got[0].time(), "12:00:00.000");
        assert_eq!(got[0].date(), "01-01");
        assert_eq!(got[0].level, crate::model::LevelMask::I);
        assert_eq!(got[0].message(), "first");
        assert_eq!(got[1].message(), "second");
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
        // `/dev/full` is unavailable in some sandboxes and containers (seccomp,
        // device cgroup, non-Linux emulation layers) even though the platform is
        // Linux. That is an environment limitation, not a recording failure, so
        // skip instead of failing the suite for it.
        let Ok(full) = OpenOptions::new().write(true).open("/dev/full") else {
            eprintln!("skipping: /dev/full is not writable in this environment");
            cleanup(&state);
            return;
        };
        writer.file = full;
        writer.record(Cursor::new(b"cannot fit\n"));
        assert!(state.finished.load(Ordering::Acquire));
        assert!(state.stop.load(Ordering::Acquire));
        assert!(state.error.lock_recover().is_some());
        assert_eq!(state.written.load(Ordering::Acquire), 0);
        cleanup(&state);
    }

    #[test]
    fn filtered_export_preserves_the_original_bytes() {
        // A raw recording keeps invalid UTF-8, CRLF and a possible BOM; the
        // filtered export must copy the matching rows byte for byte instead of
        // rewriting them as lossy UTF-8.
        let writer = writer();
        let state = writer.state.clone();
        let raw: &[u8] =
            b"\xef\xbb\xbfI/Tag( 1): keep \xff\xfe bytes\r\nE/Tag( 1): drop\nI/Tag( 2): keep tail";
        writer.record(Cursor::new(raw));
        let filtered = state.path.with_extension("filtered");
        let spec = FilterSpec {
            find: vec!["keep".into()],
            ..Default::default()
        };
        assert_eq!(
            export(&state.path, raw.len() as u64, &filtered, &spec).unwrap(),
            2
        );
        assert_eq!(
            std::fs::read(&filtered).unwrap(),
            b"\xef\xbb\xbfI/Tag( 1): keep \xff\xfe bytes\r\nI/Tag( 2): keep tail",
            "invalid bytes, CRLF and the missing final newline must survive"
        );
        std::fs::remove_file(filtered).unwrap();
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
        // The original line ending is preserved: the export copies raw rows
        // instead of re-encoding them.
        assert_eq!(
            std::fs::read(&filtered).unwrap(),
            b"I/Tag( 1): keep first\r\n"
        );
        std::fs::remove_file(filtered).unwrap();
        cleanup(&state);
    }

    #[test]
    fn export_replaces_existing_output_and_preserves_it_on_failure() {
        let writer = writer();
        let state = writer.state.clone();
        writer.record(Cursor::new(b"keep\r\ndrop\n"));
        let output = state.path.with_extension("export");
        std::fs::write(&output, b"old export").unwrap();
        let filter = FilterSpec {
            find: vec!["keep".into()],
            ..Default::default()
        };
        assert_eq!(export(&state.path, 11, &output, &filter).unwrap(), 1);
        assert_eq!(std::fs::read(&output).unwrap(), b"keep\r\n");
        let tmp = crate::config::temp_sibling(&output);
        assert!(!tmp.exists());
        // Fail after opening the output, while scanning an over-limit source row.
        File::create(&state.path)
            .unwrap()
            .set_len(crate::io::MAX_INITIAL_LINE_BYTES as u64 + 1)
            .unwrap();
        assert!(export(
            &state.path,
            crate::io::MAX_INITIAL_LINE_BYTES as u64 + 1,
            &output,
            &filter
        )
        .is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"keep\r\n");
        assert!(!tmp.exists());
        // Publishing to a directory also cleans up the completed temporary file.
        let directory = state.path.with_extension("directory");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&state.path, b"keep\n").unwrap();
        assert!(export(&state.path, 5, &directory, &filter).is_err());
        assert!(directory.is_dir());
        assert!(!crate::config::temp_sibling(&directory).exists());
        std::fs::remove_dir(directory).unwrap();
        std::fs::remove_file(output).unwrap();
        cleanup(&state);
    }

    #[test]
    fn export_matches_first_line_through_a_bom_but_keeps_raw_bytes() {
        let writer = writer();
        let state = writer.state.clone();
        let bom = [0xEF, 0xBB, 0xBF];
        let bytes = [
            &bom[..],
            b"01-01 12:00:00.000 1 2 I 00400/Tag: first\n",
            b"01-01 12:00:00.001 1 2 I 00400/Tag: second\n",
        ]
        .concat();
        writer.record(Cursor::new(bytes.clone()));
        let output = state.path.with_extension("bom_export");
        // A level filter on the parsed rows: without stripping the BOM, row 1
        // would fail the timestamp parse (level V) and be silently dropped.
        let filter = FilterSpec {
            allowed_levels: Some(crate::model::LevelMask::I),
            ..Default::default()
        };
        let rows = export(&state.path, bytes.len() as u64, &output, &filter).unwrap();
        assert_eq!(rows, 2, "the BOM must not break row 1's parse");
        // The exported bytes are the raw rows, BOM included.
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        std::fs::remove_file(output).unwrap();
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
