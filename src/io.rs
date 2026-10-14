use crate::model::EncodingChoice;
use crossbeam_channel::Sender;
use encoding_rs::{CoderResult, Decoder, Encoding};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume_serial: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(windows)]
    created: u64,
    #[cfg(not(any(unix, windows)))]
    created: Option<SystemTime>,
}

fn file_identity(file: &File) -> std::io::Result<FileIdentity> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::mem::MaybeUninit;
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: `file` owns a valid Windows file handle, and `info` points to
        // writable storage of the exact structure required by this API.
        let ok =
            unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) };
        let (volume_serial, file_index) = if ok != 0 {
            // SAFETY: a nonzero return means the API initialized `info`.
            let info = unsafe { info.assume_init() };
            (
                Some(info.dwVolumeSerialNumber),
                Some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64),
            )
        } else {
            // Some remote filesystems do not provide a file index. Keep file
            // loading functional and use creation time as a weaker fallback.
            (None, None)
        };
        Ok(FileIdentity {
            volume_serial,
            file_index,
            created: metadata.creation_time(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(FileIdentity {
            created: metadata.created().ok(),
        })
    }
}

/// How the tail/follow poller should pick up data appended after the initial
/// load, decided once from the encoding chosen for that load.
pub enum Tail {
    /// `offset` is the byte position just past the end of the initial load. For
    /// UTF-8 and byte-safe legacy codepages (GBK, Shift_JIS, EUC-KR, Big5,
    /// Windows-1252) appended data is split on raw `\n`; UTF-16 is decoded into
    /// code units and split on the '\n' character (see `read_appended`).
    Append {
        offset: u64,
        enc: &'static Encoding,
        identity: FileIdentity,
    },
    /// The initial read ended with an unterminated line. Keep showing that
    /// provisional final line, but reload on the next file change so appended
    /// bytes are joined to it instead of appearing as a second row.
    ReloadOnChange {
        loaded_len: u64,
        modified: Option<SystemTime>,
        enc: &'static Encoding,
        identity: FileIdentity,
    },
}

impl Tail {
    /// Canonical name of the encoding actually used for this load (e.g. "UTF-8",
    /// "GBK", "UTF-16LE"). Lets the UI show the detected encoding rather than the
    /// user's menu choice — notably "Local" resolves to whatever was sniffed.
    pub fn encoding_name(&self) -> &'static str {
        match self {
            Tail::Append { enc, .. } | Tail::ReloadOnChange { enc, .. } => enc.name(),
        }
    }
}

/// Upper bound on how much appended data one tail poll will buffer.
///
/// A followed file can grow by an arbitrary amount between two polls (the app or
/// the OS was suspended, the poll thread was starved, a burst writer, a slow
/// network filesystem). Reading that whole growth into memory at once made the
/// peak unbounded — large enough to abort the process on allocation failure.
/// Reads are chunked at this size instead, and [`Appended::more`] tells the
/// caller to continue immediately rather than wait for the next tick.
const MAX_TAIL_READ: u64 = 64 * 1024 * 1024;

// Compile-time guard: a larger budget re-opens the unbounded-peak issue the
// chunked read fixes, so raising it has to be a deliberate, reviewed change
// rather than a stray edit. `tail_default_budget_bounds_one_poll_end_to_end`
// covers the behavioural side of the same invariant.
const _: () = assert!(
    MAX_TAIL_READ <= 64 * 1024 * 1024,
    "raising MAX_TAIL_READ re-opens the tail read memory peak"
);

/// Result of reading data appended since a previous tail offset.
pub struct Appended {
    /// New byte offset (just past the last complete line consumed).
    pub offset: u64,
    /// True if the path now points to a different file, or the same file is
    /// shorter than `offset` — the caller should reload instead of appending.
    pub truncated: bool,
    /// True while the file still holds data this call deliberately did not
    /// consume (the [`MAX_TAIL_READ`] budget was reached, or a partial line is
    /// still being held back).
    ///
    /// The caller should poll again immediately instead of sleeping, so a large
    /// backlog drains quickly while the memory per read stays bounded. `more` is
    /// never set when the call consumed nothing, so it cannot spin.
    pub more: bool,
}

/// A tail-read failure that retrying is not expected to fix.
///
/// Carried as an [`std::io::Error`] payload so the follower can recognise *our*
/// deliberately-permanent failures instead of guessing from
/// [`std::io::ErrorKind`]. `ErrorKind` is a coarse vocabulary shared with the OS:
/// a `read`/`seek` failure that happens to report `InvalidData` — or any other
/// kind — must keep the follower polling, because reloading the file can
/// genuinely recover it.
#[derive(Debug)]
pub(crate) struct PermanentTailReadError(&'static str);

impl std::fmt::Display for PermanentTailReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PermanentTailReadError {}

/// An [`std::io::Error`] explicitly marked as unfixable by retrying.
pub(crate) fn permanent_io_error(message: &'static str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::OutOfMemory,
        PermanentTailReadError(message),
    )
}

/// Reserve a buffer for `want` bytes without ever aborting the process.
///
/// `Vec::with_capacity` routes an allocation failure to `handle_alloc_error`,
/// which aborts: no unwinding, no error message, unsaved work lost.
/// `try_reserve_exact` turns the same refusal into an `Err`. On `Err` no buffer
/// is handed back, so a failed reservation can never be read from or written to.
pub(crate) fn reserve_buffer(want: usize) -> std::io::Result<Vec<u8>> {
    let mut raw: Vec<u8> = Vec::new();
    raw.try_reserve_exact(want)
        .map_err(|_| permanent_io_error("not enough memory to buffer appended log data"))?;
    Ok(raw)
}

/// Buffer at most `want` bytes of the appended range starting at `offset`.
///
/// Reserves exactly `want` bytes up front and never reads past it, so a writer
/// that appends faster than we read cannot make the buffer grow without bound.
fn read_appended_bytes(file: &mut File, offset: u64, want: usize) -> std::io::Result<Vec<u8>> {
    let mut raw = reserve_buffer(want)?;
    // `reserve_buffer` already reserved the capacity, so this resize cannot
    // reallocate — and returning early on `Err` above means the buffer is never
    // touched when the reservation failed.
    raw.resize(want, 0);
    file.seek(SeekFrom::Start(offset))?;
    let mut filled = 0usize;
    while filled < want {
        match file.read(&mut raw[filled..]) {
            Ok(0) => break, // EOF (or the file was truncated under us)
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    raw.truncate(filled);
    Ok(raw)
}

/// Largest prefix of `raw` that ends on a complete character boundary for `enc`.
///
/// Used when an over-long line has to be split at the read budget: cutting a
/// character in half decodes its head to U+FFFD *and* leaves the orphaned tail
/// bytes to decay to a second U+FFFD on the next read, corrupting two rows
/// instead of one. The unconsumed tail is simply left for the next read.
///
/// Never returns 0 for non-empty `raw` unless the chunk genuinely holds no
/// complete character, so a caller that falls back to consuming the chunk can
/// always make progress.
fn floor_char_boundary(enc: &'static Encoding, raw: &[u8]) -> usize {
    if enc == encoding_rs::UTF_8 {
        return match std::str::from_utf8(raw) {
            Ok(_) => raw.len(),
            // `valid_up_to` is by definition the end of the last complete
            // character. A genuinely malformed byte lands here too; its position
            // is still the right place to stop and leave the rest for next time.
            Err(e) => e.valid_up_to(),
        };
    }
    if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
        // Whole code units only; a trailing half code unit is held back.
        let units = raw.len() & !1;
        // A trailing high surrogate is probably paired with the first code unit
        // of the next chunk. Only hold it back when a unit precedes it: an empty
        // prefix would leave the offset unable to advance.
        if units >= 4 && is_high_surrogate(enc, &raw[units - 2..units]) {
            return units - 2;
        }
        return units;
    }
    // Legacy East-Asian codepages (GBK/Shift_JIS/EUC-KR/Big5, plus windows-1252)
    // keep the raw chunk: locating their boundaries needs decoder state that
    // `encoding_rs` does not expose, and splitting one costs a single U+FFFD in a
    // line already larger than the read budget.
    raw.len()
}

/// Whether the two bytes of one UTF-16 code unit are a high surrogate.
fn is_high_surrogate(enc: &'static Encoding, unit: &[u8]) -> bool {
    debug_assert_eq!(unit.len(), 2);
    let value = if enc == encoding_rs::UTF_16LE {
        u16::from_le_bytes([unit[0], unit[1]])
    } else {
        u16::from_be_bytes([unit[0], unit[1]])
    };
    (0xD800..=0xDBFF).contains(&value)
}

/// Stream a UTF-8 (or BOM-detected UTF-16) file's lines into `tx`.
///
/// Returns `Ok(Tail)` describing how to follow subsequent appends on clean EOF
/// (or when a newer source epoch supersedes this load); returns `Err` only on a
/// genuine mid-stream read failure, so the caller can distinguish "finished"
/// from "truncated by an I/O error" and surface the latter instead of
/// presenting a partial file as whole.
pub fn send_utf8_lines(
    file: File,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
) -> std::io::Result<Tail> {
    let identity = file_identity(&file)?;
    let mut reader = BufReader::new(file);
    let bom = reader.fill_buf()?;
    if bom.starts_with(&[0xFF, 0xFE]) || bom.starts_with(&[0xFE, 0xFF]) {
        // UTF-16 BOM detected: pick LE/BE and delegate to the decoded path.
        let is_le = bom.starts_with(&[0xFF, 0xFE]);
        let mut file2 = reader.into_inner();
        // fill_buf() advanced the file position (up to the BufReader capacity)
        // and into_inner() discards that buffer, so rewind — otherwise the
        // delegate reads from ~8 KiB in and the file's first chunk is skipped.
        file2.seek(SeekFrom::Start(0))?;
        let enc = if is_le {
            encoding_rs::UTF_16LE
        } else {
            encoding_rs::UTF_16BE
        };
        return send_decoded_lines_with_enc(file2, tx, epoch, source_epoch, enc);
    }

    let mut buf = Vec::new();
    let mut first = true;
    let mut read_bytes: u64 = 0;
    let mut has_partial_line = false;
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            let metadata = reader.get_ref().metadata()?;
            return Ok(if has_partial_line {
                Tail::ReloadOnChange {
                    loaded_len: read_bytes,
                    modified: metadata.modified().ok(),
                    enc: encoding_rs::UTF_8,
                    identity,
                }
            } else {
                Tail::Append {
                    offset: read_bytes,
                    enc: encoding_rs::UTF_8,
                    identity,
                }
            });
        }
        read_bytes += n as u64;
        if source_epoch.load(Ordering::Acquire) != epoch {
            return Ok(Tail::Append {
                offset: read_bytes,
                enc: encoding_rs::UTF_8,
                identity,
            });
        }
        if buf.last() != Some(&b'\n') {
            has_partial_line = true;
        }
        // Fast path: valid UTF-8 -> borrow & trim on the slice, no lossy scan.
        let line: String = if let Ok(s) = std::str::from_utf8(&buf[..n]) {
            let mut s = s;
            if first {
                first = false;
                s = s.strip_prefix('\u{feff}').unwrap_or(s);
            }
            s.trim_end_matches(['\n', '\r']).to_string()
        } else {
            let mut line = String::from_utf8_lossy(&buf).into_owned();
            if first {
                first = false;
                if line.starts_with('\u{feff}') {
                    line.remove(0);
                }
            }
            while line.ends_with(['\n', '\r']) {
                line.pop();
            }
            line
        };
        if !send_throttled(&tx, epoch, line) {
            return Ok(Tail::Append {
                offset: read_bytes,
                enc: encoding_rs::UTF_8,
                identity,
            });
        }
    }
}

pub fn send_decoded_lines(
    mut file: File,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    choice: EncodingChoice,
) -> std::io::Result<Tail> {
    let enc = match choice {
        EncodingChoice::Utf8 => encoding_rs::UTF_8,
        EncodingChoice::Local => {
            // UTF-8 is self-validating: a legacy-encoded (GBK/Shift_JIS/…) file's
            // multibyte sequences almost never form valid UTF-8, while real UTF-8
            // virtually never validates by accident. So sniff a prefix first —
            // decode modern UTF-8 logs correctly and only fall back to the
            // locale's legacy codepage when the bytes are clearly not UTF-8. This
            // stops "Local" from mojibaking a UTF-8 file (the common case today).
            if looks_like_utf8(&mut file)? {
                encoding_rs::UTF_8
            } else {
                let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
                pick_local_encoding(&locale)
            }
        }
    };
    send_decoded_lines_with_enc(file, tx, epoch, source_epoch, enc)
}

/// Peek up to 64 KiB from the start of `file` and report whether it is valid
/// UTF-8, then rewind so the caller reads from the beginning. A truncated
/// multibyte sequence at the sniff boundary is tolerated: if the only error is
/// an incomplete tail, the prefix is still treated as UTF-8.
fn looks_like_utf8(file: &mut File) -> std::io::Result<bool> {
    const SNIFF: usize = 64 * 1024;
    let mut buf = vec![0u8; SNIFF];
    let mut read = 0;
    // A single read() may return a short count; loop until the buffer is full or
    // EOF so a large file is actually sniffed on its first 64 KiB, not ~8 KiB.
    while read < buf.len() {
        let n = file.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    file.seek(SeekFrom::Start(0))?;
    let bytes = &buf[..read];
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(true),
        // `valid_up_to` bytes decoded cleanly; if the remaining error is only a
        // multibyte char cut off by the 64 KiB boundary (no explicit error_len),
        // the content is still UTF-8 — the cut is our artifact, not the file's.
        Err(e) => Ok(e.error_len().is_none() && e.valid_up_to() > 0),
    }
}

fn send_decoded_lines_with_enc(
    file: File,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    enc: &'static Encoding,
) -> std::io::Result<Tail> {
    let identity = file_identity(&file)?;
    let mut reader = BufReader::with_capacity(8192, file);
    let mut decoder = enc.new_decoder();
    // Accumulate decoded text across chunks so we can split into lines only when
    // a full line is available — avoids splitting a multibyte character mid-sequence.
    let mut text_buf = String::with_capacity(8192);
    let mut raw_buf = vec![0u8; 8192];
    let mut read_bytes: u64 = 0;
    // All encodings here tail incrementally: UTF-8 and the byte-safe legacy
    // codepages split on raw `\n`, and UTF-16 is handled specially in
    // `read_appended` (decode code units, split on the '\n' character).
    let is_utf16 = enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE;
    let tail_for = |bytes: u64| {
        // UTF-16 tails by whole code units, so a base offset caught mid-unit
        // (odd — the file was flushed half a code unit) would misalign every
        // later read and stall the tail. Round down to a whole code unit.
        let offset = if is_utf16 { bytes & !1 } else { bytes };
        Tail::Append {
            offset,
            enc,
            identity,
        }
    };
    loop {
        let n = reader.read(&mut raw_buf)?;
        if n == 0 {
            break;
        }
        read_bytes += n as u64;
        if source_epoch.load(Ordering::Acquire) != epoch {
            return Ok(tail_for(read_bytes));
        }
        decode_chunk(
            &mut decoder,
            &raw_buf[..n],
            false,
            &mut text_buf,
            &tx,
            epoch,
        );
        if source_epoch.load(Ordering::Acquire) != epoch {
            return Ok(tail_for(read_bytes));
        }
    }
    // Final flush: drain any remaining buffered bytes from the decoder.
    decode_chunk(&mut decoder, b"", true, &mut text_buf, &tx, epoch);
    // Emit any remaining text without a trailing newline as a final line.
    let has_partial_line = !text_buf.is_empty();
    if has_partial_line {
        if source_epoch.load(Ordering::Acquire) != epoch {
            return Ok(tail_for(read_bytes));
        }
        let line = text_buf.trim_end_matches(['\r', '\n']).to_string();
        // Match the UTF-8 reader's `read_until` behavior: a final logical line
        // is preserved even if CR/LF normalization leaves it empty (for example
        // a file ending in a lone `\r`).
        if !send_throttled(&tx, epoch, line) {
            return Ok(tail_for(read_bytes));
        }
    }
    if has_partial_line {
        let metadata = reader.get_ref().metadata()?;
        Ok(Tail::ReloadOnChange {
            loaded_len: read_bytes,
            modified: metadata.modified().ok(),
            enc,
            identity,
        })
    } else {
        Ok(tail_for(read_bytes))
    }
}

/// Read bytes appended to `path` since `offset` and send each COMPLETE line
/// (terminated by `\n`) via `tx`, decoded with `enc`. A trailing partial line
/// (no newline yet — a log entry mid-write) is intentionally left unread so it
/// isn't shown truncated; it will be picked up once its newline arrives. This
/// is the "strategy A" tailing behavior.
///
/// Returns the new offset (advanced only past complete lines) and whether the
/// file is now shorter than `offset` (truncated → caller should reload).
#[cfg(test)]
pub fn read_appended(
    path: &Path,
    offset: u64,
    enc: &'static Encoding,
    tx: &Sender<(u64, String)>,
    epoch: u64,
) -> std::io::Result<Appended> {
    read_appended_checked(path, offset, enc, tx, epoch, None)
}

pub(crate) fn read_appended_checked(
    path: &Path,
    offset: u64,
    enc: &'static Encoding,
    tx: &Sender<(u64, String)>,
    epoch: u64,
    expected_identity: Option<FileIdentity>,
) -> std::io::Result<Appended> {
    read_appended_with_budget(
        path,
        offset,
        enc,
        tx,
        epoch,
        expected_identity,
        MAX_TAIL_READ,
    )
}

/// [`read_appended_checked`] with an explicit per-call read budget.
///
/// Split out so tests can drive chunking, the `more` backlog, and over-long
/// lines with small inputs instead of multi-megabyte files.
fn read_appended_with_budget(
    path: &Path,
    offset: u64,
    enc: &'static Encoding,
    tx: &Sender<(u64, String)>,
    epoch: u64,
    expected_identity: Option<FileIdentity>,
    max_read: u64,
) -> std::io::Result<Appended> {
    let mut file = File::open(path)?;
    if expected_identity.is_some_and(|expected| file_identity(&file).ok() != Some(expected)) {
        return Ok(Appended {
            offset,
            truncated: true,
            more: false,
        });
    }
    let len = file.seek(SeekFrom::End(0))?;
    if len < offset {
        return Ok(Appended {
            offset,
            truncated: true,
            more: false,
        });
    }
    if len == offset {
        return Ok(Appended {
            offset,
            truncated: false,
            more: false,
        });
    }
    // Bounded, chunked read: buffer at most `max_read` of the growth, so the
    // peak no longer scales with how much the file grew between two polls. The
    // budget is capped at `MAX_TAIL_READ`, which fits in `usize` on every
    // supported target, so the range conversion below cannot actually overflow;
    // keeping it total avoids inventing an `InvalidData` error that a caller
    // could mistake for something a reload would recover.
    let budget = max_read.min(MAX_TAIL_READ);
    let want = usize::try_from((len - offset).min(budget)).unwrap_or(usize::MAX);
    let raw = read_appended_bytes(&mut file, offset, want)?;
    // The file held more bytes than this call was willing to read. Together with
    // an unterminated line below, that means a single line longer than the budget.
    let unconsumed_beyond = (offset + raw.len() as u64) < len;

    // UTF-16: `0x0A` also occurs as a byte inside unrelated code units, so we
    // can't split on raw newline bytes. Decode whole 2-byte code units, split on
    // the '\n' *character*, and advance the offset by the UTF-16 byte length of
    // what we consumed. (This lets UTF-16 files tail incrementally instead of
    // reloading the whole file on every size change.)
    if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
        // Whole code units only, and a trailing high surrogate is left for the
        // next read (see `floor_char_boundary`).
        let usable = floor_char_boundary(enc, &raw);
        // decode_without_bom_handling: a plain decode() would BOM-sniff the chunk
        // and, if it happens to start with BOM-like bytes (a real U+FEFF char, or
        // 0xFFFE), drop a char or switch encoding — drifting the byte offset.
        let (text, _) = enc.decode_without_bom_handling(&raw[..usable]);
        let consumed_chars = match text.rfind('\n') {
            Some(nl) => nl + 1,
            // No complete line in this chunk, and the file holds more than we
            // read: the line exceeds the budget. Emit what we buffered so the
            // tail keeps moving instead of retrying the same read forever.
            None if unconsumed_beyond => text.len(),
            None => {
                return Ok(Appended {
                    offset,
                    truncated: false,
                    more: false,
                })
            }
        };
        if consumed_chars == 0 {
            // Nothing consumable (e.g. only a stray half code unit): report no
            // progress so the caller does not spin.
            return Ok(Appended {
                offset,
                truncated: false,
                more: false,
            });
        }
        let consumed = &text[..consumed_chars];
        for chunk in consumed.split_inclusive('\n') {
            let line = chunk.trim_end_matches(['\r', '\n']).to_string();
            if !send_throttled(tx, epoch, line) {
                return Ok(Appended {
                    offset,
                    truncated: false,
                    more: false,
                });
            }
        }
        let consumed_bytes = consumed.encode_utf16().count() as u64 * 2;
        let new_offset = offset + consumed_bytes;
        return Ok(Appended {
            offset: new_offset,
            truncated: false,
            more: new_offset < len,
        });
    }

    // Only consume up to the last newline; bytes after it are a partial line.
    let end = match raw.iter().rposition(|&b| b == b'\n') {
        Some(nl) => nl + 1,
        // No complete line in this chunk, and the file holds more than we read:
        // the line exceeds the budget. Emit a prefix so the tail keeps moving,
        // backing off to a character boundary so the split cannot halve a
        // character; the unconsumed tail is picked up by the next read.
        None if unconsumed_beyond => {
            let safe = floor_char_boundary(enc, &raw);
            if safe == 0 {
                // The chunk is nothing but an incomplete character (only
                // reachable when the budget is smaller than one character).
                // Waiting cannot complete it because the file already holds more
                // bytes, so consume it lossily: one bad row beats an offset that
                // never advances.
                raw.len()
            } else {
                safe
            }
        }
        None => {
            // No complete line yet — leave the offset untouched.
            return Ok(Appended {
                offset,
                truncated: false,
                more: false,
            });
        }
    };
    if end == 0 {
        // The read raced a truncation and returned nothing: no progress.
        return Ok(Appended {
            offset,
            truncated: false,
            more: false,
        });
    }
    let complete = &raw[..end];
    for chunk in complete.split_inclusive(|&b| b == b'\n') {
        let (cow, _, _) = enc.decode(chunk);
        let line = cow.trim_end_matches(['\r', '\n']).to_string();
        if !send_throttled(tx, epoch, line) {
            return Ok(Appended {
                offset,
                truncated: false,
                more: false,
            });
        }
    }
    let new_offset = offset + complete.len() as u64;
    Ok(Appended {
        offset: new_offset,
        truncated: false,
        more: new_offset < len,
    })
}

pub(crate) fn file_changed_since(
    path: &Path,
    loaded_len: u64,
    modified: Option<SystemTime>,
    identity: FileIdentity,
) -> std::io::Result<bool> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    Ok(metadata.len() != loaded_len
        || file_identity(&file)? != identity
        || metadata.modified().ok() != modified)
}

/// Decode all of `input`, growing `text_buf` or flushing complete lines when
/// the decoder reports a full output buffer. `decode_to_string` deliberately
/// does not reallocate its destination; ignoring `OutputFull` would otherwise
/// discard the unconsumed suffix of a long log line.
fn decode_chunk(
    decoder: &mut Decoder,
    mut input: &[u8],
    last: bool,
    text_buf: &mut String,
    tx: &Sender<(u64, String)>,
    epoch: u64,
) {
    loop {
        if text_buf.len() == text_buf.capacity() {
            text_buf.reserve(8192);
        }
        let (result, read, _) = decoder.decode_to_string(input, text_buf, last);
        input = &input[read..];

        // Drain complete lines to free space before allocating more. A single
        // very long line has no newline to drain, in which case reserve grows
        // the backing buffer and the next iteration resumes the same input.
        let consumed = flush_lines(text_buf, tx, epoch);
        if consumed > 0 {
            text_buf.drain(..consumed);
        }

        match result {
            CoderResult::InputEmpty => return,
            CoderResult::OutputFull if consumed == 0 => text_buf.reserve(8192),
            CoderResult::OutputFull => {}
        }
    }
}

/// Scan `buf` for complete lines (terminated by `\n`), send each via `tx`,
/// and return the number of bytes consumed (the processed prefix length).
/// The caller drains that prefix once — O(n) per chunk instead of O(n²).
fn flush_lines(buf: &str, tx: &Sender<(u64, String)>, epoch: u64) -> usize {
    let bytes = buf.as_bytes();
    let mut consumed = 0usize;
    let mut scan = 0usize;
    while scan < bytes.len() {
        if bytes[scan] == b'\n' {
            let line = buf[consumed..scan].trim_end_matches(['\r', '\n']);
            if !send_throttled(tx, epoch, line.to_string()) {
                return consumed;
            }
            consumed = scan + 1;
        }
        scan += 1;
    }
    consumed
}

/// Lines allowed to sit queued ahead of the ingest side before a *file* reader
/// waits.
///
/// The log channel is unbounded so the device reader never blocks on it (see
/// `transport::log_channel`) — there, waiting would let adb's pipe fill and cost
/// us logs on the device. The file paths have no pipe to protect, so they apply
/// their own backpressure instead: a fast disk read must not queue the whole file
/// as Strings ahead of the slower parse/append step.
pub(crate) const MAX_QUEUED_LINES: usize = 8192;

/// Hand a line to the ingest side, waiting while it is already too far behind.
///
/// Returns false when the receiver is gone.
fn send_throttled(tx: &Sender<(u64, String)>, epoch: u64, line: String) -> bool {
    while tx.len() >= MAX_QUEUED_LINES {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    tx.send((epoch, line)).is_ok()
}

pub fn pick_local_encoding(locale: &str) -> &'static Encoding {
    let low = locale.to_lowercase();
    if low.starts_with("zh") {
        encoding_rs::GBK
    } else if low.starts_with("ja") {
        encoding_rs::SHIFT_JIS
    } else if low.starts_with("ko") {
        encoding_rs::EUC_KR
    } else {
        encoding_rs::WINDOWS_1252
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    #[test]
    fn pick_local_encoding_maps_locales() {
        assert_eq!(pick_local_encoding("zh-CN").name(), "GBK");
        assert_eq!(pick_local_encoding("zh_TW.UTF-8").name(), "GBK");
        assert_eq!(pick_local_encoding("ja_JP").name(), "Shift_JIS");
        assert_eq!(pick_local_encoding("ko_KR").name(), "EUC-KR");
        assert_eq!(pick_local_encoding("en-US").name(), "windows-1252");
        assert_eq!(pick_local_encoding("fr_FR").name(), "windows-1252");
    }

    #[test]
    fn utf16le_file_detected_and_delegated() {
        let tmp = std::env::temp_dir().join(format!("lf_utf16le_{}.log", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            let bom = [0xFF, 0xFE];
            let text: Vec<u8> = "hello\nworld"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect();
            f.write_all(&bom).unwrap();
            f.write_all(&text).unwrap();
        }
        let (tx, rx) = crossbeam_channel::bounded(16);
        let epoch = Arc::new(AtomicU64::new(1));
        let file = std::fs::File::open(&tmp).unwrap();
        // send_utf8_lines detects the UTF-16 BOM and delegates to the decoded
        // path, which must read from the *start* of the file (the BOM detection
        // uses fill_buf, which advances the position — the delegate has to rewind).
        let tail = send_utf8_lines(file, tx, 1, epoch).unwrap();
        let lines: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(
            lines,
            vec!["hello".to_string(), "world".to_string()],
            "UTF-16 content must be decoded from the start, not skipped"
        );
        assert_eq!(tail.encoding_name(), "UTF-16LE");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn epoch_cancels_mid_read() {
        let tmp = std::env::temp_dir().join(format!("lf_epoch_{}.log", std::process::id()));
        // Write a file with many lines
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            for i in 0..1000 {
                writeln!(f, "line {i}").unwrap();
            }
        }
        let (tx, rx) = crossbeam_channel::bounded(2048);
        let epoch = Arc::new(AtomicU64::new(1));
        // Immediately bump epoch so the reader sees a mismatch after first line
        epoch.store(2, std::sync::atomic::Ordering::Release);
        let file = std::fs::File::open(&tmp).unwrap();
        let _ = send_utf8_lines(file, tx, 1, epoch);
        let lines: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        let _ = std::fs::remove_file(&tmp);
        // Should have stopped early (at most 1 line read before epoch check)
        assert!(lines.len() <= 1, "expected <=1 lines, got {}", lines.len());
    }

    #[test]
    fn decoded_reader_preserves_empty_final_line_like_utf8_reader() {
        let tmp =
            std::env::temp_dir().join(format!("lf_final_empty_line_{}.log", std::process::id()));
        std::fs::write(&tmp, b"first\n\r").unwrap();

        let read_lines = |decoded: bool| {
            let (tx, rx) = crossbeam_channel::bounded(16);
            let epoch = Arc::new(AtomicU64::new(1));
            let file = std::fs::File::open(&tmp).unwrap();
            if decoded {
                send_decoded_lines_with_enc(file, tx, 1, epoch, encoding_rs::UTF_8).unwrap();
            } else {
                send_utf8_lines(file, tx, 1, epoch).unwrap();
            }
            rx.try_iter().map(|(_, line)| line).collect::<Vec<_>>()
        };

        let utf8_lines = read_lines(false);
        let decoded_lines = read_lines(true);
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(utf8_lines, vec!["first", ""]);
        assert_eq!(decoded_lines, utf8_lines);
    }

    #[test]
    fn decoded_reader_preserves_long_line_larger_than_initial_buffer() {
        let tmp =
            std::env::temp_dir().join(format!("lf_long_decoded_line_{}.log", std::process::id()));
        let line = "日志".repeat(16 * 1024);
        let (encoded, _, _) = encoding_rs::GBK.encode(&line);
        std::fs::write(&tmp, encoded.as_ref()).unwrap();

        let (tx, rx) = crossbeam_channel::bounded(4);
        let epoch = Arc::new(AtomicU64::new(1));
        let file = std::fs::File::open(&tmp).unwrap();
        send_decoded_lines_with_enc(file, tx, 1, epoch, encoding_rs::GBK).unwrap();
        let lines: Vec<String> = rx.try_iter().map(|(_, line)| line).collect();
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(lines, vec![line]);
    }

    // Helper: run the full `Local` path against a file's raw bytes.
    fn decode_via_local(bytes: &[u8]) -> Vec<String> {
        let tmp = std::env::temp_dir().join(format!(
            "lf_local_path_{}_{}.log",
            std::process::id(),
            bytes.len()
        ));
        std::fs::write(&tmp, bytes).unwrap();
        let (tx, rx) = crossbeam_channel::bounded(64);
        let epoch = Arc::new(AtomicU64::new(1));
        let file = std::fs::File::open(&tmp).unwrap();
        send_decoded_lines(file, tx, 1, epoch, EncodingChoice::Local).unwrap();
        let lines: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        let _ = std::fs::remove_file(&tmp);
        lines
    }

    #[test]
    fn local_sniffs_utf8_and_does_not_mojibake() {
        // A real UTF-8 file must decode correctly via Local even on a zh/ja/ko
        // locale (where the legacy guess would otherwise corrupt it).
        let line = "中文日志 テスト 한국어 abc";
        let lines = decode_via_local(line.as_bytes());
        assert_eq!(lines, vec![line.to_string()]);
    }

    #[test]
    fn local_falls_back_to_legacy_for_non_utf8() {
        // GBK bytes are not valid UTF-8, so the sniff fails and Local uses the
        // locale codepage. On CI the locale may not be zh; assert only that the
        // GBK path is taken when it is, otherwise that we didn't crash/empty.
        let line = "中文日志 abc";
        let (gbk, _, _) = encoding_rs::GBK.encode(line);
        let lines = decode_via_local(gbk.as_ref());
        assert_eq!(lines.len(), 1);
        let locale = sys_locale::get_locale().unwrap_or_default();
        if locale.to_lowercase().starts_with("zh") {
            assert_eq!(lines, vec![line.to_string()]);
        }
    }

    #[test]
    fn looks_like_utf8_true_for_ascii_and_utf8() {
        let mk = |bytes: &[u8]| {
            let tmp = std::env::temp_dir().join(format!(
                "lf_sniff_{}_{}.bin",
                std::process::id(),
                bytes.len()
            ));
            std::fs::write(&tmp, bytes).unwrap();
            let mut f = std::fs::File::open(&tmp).unwrap();
            let r = looks_like_utf8(&mut f).unwrap();
            // Sniff must rewind: a full read after it still sees all bytes.
            let mut all = Vec::new();
            f.read_to_end(&mut all).unwrap();
            let _ = std::fs::remove_file(&tmp);
            (r, all.len())
        };
        let (ok, len) = mk("plain ascii\nsecond".as_bytes());
        assert!(ok);
        assert_eq!(len, "plain ascii\nsecond".len(), "must rewind after sniff");

        let (ok, _) = mk("多字节 UTF-8 内容".as_bytes());
        assert!(ok);

        // GBK bytes → not valid UTF-8.
        let (gbk, _, _) = encoding_rs::GBK.encode("中文");
        let (ok, _) = mk(gbk.as_ref());
        assert!(!ok);
    }

    #[test]
    fn looks_like_utf8_tolerates_truncated_tail() {
        // A valid multibyte char split by the sniff boundary must still count as
        // UTF-8 (error_len is None → incomplete tail, not a real error).
        let mut bytes = "ok ".as_bytes().to_vec();
        let full = "中".as_bytes(); // 3 bytes: E4 B8 AD
        bytes.extend_from_slice(&full[..2]); // drop the last continuation byte
        let tmp = std::env::temp_dir().join(format!("lf_trunc_{}.bin", std::process::id()));
        std::fs::write(&tmp, &bytes).unwrap();
        let mut f = std::fs::File::open(&tmp).unwrap();
        assert!(looks_like_utf8(&mut f).unwrap());
        let _ = std::fs::remove_file(&tmp);
    }

    fn unique_tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lf_{}_{}.log", tag, std::process::id()))
    }

    #[test]
    fn read_appended_streams_only_complete_lines() {
        // Strategy A: a trailing partial line (no newline) is NOT emitted and
        // the offset does not advance past it, so it's picked up once completed.
        let tmp = unique_tmp("append");
        std::fs::write(&tmp, b"one\ntwo\n").unwrap();
        let (tx, rx) = crossbeam_channel::bounded(16);

        let a = read_appended(&tmp, 0, encoding_rs::UTF_8, &tx, 1).unwrap();
        assert!(!a.truncated);
        assert_eq!(a.offset, 8);
        let got: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(got, vec!["one".to_string(), "two".to_string()]);

        // Append a complete line plus a partial one.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(b"three\npar").unwrap();
        }
        let a2 = read_appended(&tmp, a.offset, encoding_rs::UTF_8, &tx, 1).unwrap();
        let got2: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(got2, vec!["three".to_string()], "partial 'par' withheld");
        assert_eq!(a2.offset, 8 + 6, "offset stops before the partial line");

        // Completing the partial line emits it.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(b"tial\n").unwrap();
        }
        let a3 = read_appended(&tmp, a2.offset, encoding_rs::UTF_8, &tx, 1).unwrap();
        let got3: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(got3, vec!["partial".to_string()]);
        assert!(!a3.truncated);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn read_appended_tails_utf16le_incrementally() {
        // UTF-16 now tails by append (was full-reload-on-change). Verify complete
        // lines are emitted, a partial line is withheld, and the offset advances
        // by the correct UTF-16 byte count.
        let enc16 =
            |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect() };
        let tmp = unique_tmp("append_u16");
        let initial = enc16("one\ntwo\n"); // 16 bytes
        std::fs::write(&tmp, &initial).unwrap();
        let off0 = initial.len() as u64;
        let (tx, rx) = crossbeam_channel::bounded(16);

        // Append a complete line plus a partial one (no newline yet).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(&enc16("three\npar")).unwrap();
        }
        let a = read_appended(&tmp, off0, encoding_rs::UTF_16LE, &tx, 1).unwrap();
        let got: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(got, vec!["three".to_string()], "partial 'par' withheld");
        assert_eq!(
            a.offset,
            off0 + enc16("three\n").len() as u64,
            "offset stops before the partial line"
        );
        assert!(!a.truncated);

        // Completing the partial line emits it whole (not split).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(&enc16("tial\n")).unwrap();
        }
        let a2 = read_appended(&tmp, a.offset, encoding_rs::UTF_16LE, &tx, 1).unwrap();
        let got2: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(got2, vec!["partial".to_string()]);
        assert!(!a2.truncated);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn read_appended_reports_truncation() {
        let tmp = unique_tmp("trunc");
        std::fs::write(&tmp, b"aaaa\nbbbb\n").unwrap(); // 10 bytes
        let (tx, _rx) = crossbeam_channel::bounded(16);
        // Pretend we'd already consumed 10 bytes, then the file shrinks.
        std::fs::write(&tmp, b"x\n").unwrap(); // now 2 bytes < 10
        let a = read_appended(&tmp, 10, encoding_rs::UTF_8, &tx, 1).unwrap();
        assert!(a.truncated, "file shorter than offset ⇒ truncated");
        assert_eq!(a.offset, 10, "offset unchanged on truncation");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn unterminated_initial_line_is_reloaded_when_completed() {
        let tmp = unique_tmp("partial_initial");
        std::fs::write(&tmp, b"partial").unwrap();

        let (tx, rx) = crossbeam_channel::bounded(16);
        let epoch = Arc::new(AtomicU64::new(1));
        let tail =
            send_utf8_lines(std::fs::File::open(&tmp).unwrap(), tx, 1, epoch.clone()).unwrap();
        assert_eq!(
            rx.try_iter().map(|(_, line)| line).collect::<Vec<_>>(),
            vec!["partial".to_string()]
        );
        let (loaded_len, modified, identity) = match tail {
            Tail::ReloadOnChange {
                loaded_len,
                modified,
                identity,
                ..
            } => (loaded_len, modified, identity),
            Tail::Append { .. } => panic!("unterminated line must watch for a reload"),
        };
        assert_eq!(loaded_len, b"partial".len() as u64);

        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            file.write_all(b" line\n").unwrap();
        }
        assert!(file_changed_since(&tmp, loaded_len, modified, identity).unwrap());

        // The reload sees the original prefix and appended suffix as one row.
        let (tx, rx) = crossbeam_channel::bounded(16);
        let _tail = send_utf8_lines(std::fs::File::open(&tmp).unwrap(), tx, 1, epoch).unwrap();
        assert_eq!(
            rx.try_iter().map(|(_, line)| line).collect::<Vec<_>>(),
            vec!["partial line".to_string()]
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn replacement_file_at_old_offset_is_detected_as_rotation() {
        let tmp = unique_tmp("rotation_current");
        let backup = unique_tmp("rotation_backup");
        let initial = b"old file contents\n";
        std::fs::write(&tmp, initial).unwrap();

        let (tx, _rx) = crossbeam_channel::bounded(16);
        let epoch = Arc::new(AtomicU64::new(1));
        let tail =
            send_utf8_lines(std::fs::File::open(&tmp).unwrap(), tx.clone(), 1, epoch).unwrap();
        let (offset, identity) = match tail {
            Tail::Append {
                offset, identity, ..
            } => (offset, identity),
            Tail::ReloadOnChange { .. } => panic!("newline-terminated file should append"),
        };

        std::fs::rename(&tmp, &backup).unwrap();
        let replacement = format!("new file header\n{}\n", "x".repeat(offset as usize));
        assert!(replacement.len() as u64 >= offset);
        std::fs::write(&tmp, replacement).unwrap();

        let result =
            read_appended_checked(&tmp, offset, encoding_rs::UTF_8, &tx, 1, Some(identity))
                .unwrap();
        assert!(result.truncated, "replacement must trigger a full reload");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn read_appended_no_growth_is_noop() {
        let tmp = unique_tmp("nogrow");
        std::fs::write(&tmp, b"a\nb\n").unwrap();
        let (tx, rx) = crossbeam_channel::bounded(16);
        let a = read_appended(&tmp, 4, encoding_rs::UTF_8, &tx, 1).unwrap();
        assert!(!a.truncated);
        assert_eq!(a.offset, 4);
        assert!(rx.try_iter().next().is_none(), "no lines when unchanged");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn read_appended_decodes_gbk_lines() {
        let tmp = unique_tmp("gbk_tail");
        let (gbk, _, _) = encoding_rs::GBK.encode("日志\n");
        std::fs::write(&tmp, gbk.as_ref()).unwrap();
        let (tx, rx) = crossbeam_channel::bounded(16);
        let a = read_appended(&tmp, 0, encoding_rs::GBK, &tx, 9).unwrap();
        assert!(!a.truncated);
        let got: Vec<(u64, String)> = rx.try_iter().collect();
        assert_eq!(got, vec![(9, "日志".to_string())]);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn send_utf8_lines_returns_append_tail_with_byte_offset() {
        let tmp = unique_tmp("tail_utf8");
        std::fs::write(&tmp, b"hello\nworld\n").unwrap(); // 12 bytes
        let (tx, _rx) = crossbeam_channel::bounded(16);
        let epoch = Arc::new(AtomicU64::new(1));
        let file = std::fs::File::open(&tmp).unwrap();
        let tail = send_utf8_lines(file, tx, 1, epoch).unwrap();
        match tail {
            Tail::Append { offset, enc, .. } => {
                assert_eq!(offset, 12);
                assert_eq!(enc.name(), "UTF-8");
            }
            Tail::ReloadOnChange { .. } => panic!("newline-terminated file should append"),
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn tail_encoding_name_reports_actual_encoding() {
        assert_eq!(
            Tail::Append {
                offset: 0,
                enc: encoding_rs::GBK,
                identity: FileIdentity::default(),
            }
            .encoding_name(),
            "GBK"
        );
        assert_eq!(
            Tail::Append {
                offset: 0,
                enc: encoding_rs::UTF_16LE,
                identity: FileIdentity::default(),
            }
            .encoding_name(),
            "UTF-16LE"
        );
    }

    #[test]
    fn local_load_reports_utf8_when_sniffed() {
        // A UTF-8 file opened as Local must report UTF-8 (not "Local"), proving
        // the status bar would show the sniffed encoding.
        let tmp = unique_tmp("local_reports");
        std::fs::write(&tmp, "中文 abc\n".as_bytes()).unwrap();
        let (tx, _rx) = crossbeam_channel::bounded(16);
        let epoch = Arc::new(AtomicU64::new(1));
        let file = std::fs::File::open(&tmp).unwrap();
        let tail = send_decoded_lines(file, tx, 1, epoch, EncodingChoice::Local).unwrap();
        assert_eq!(tail.encoding_name(), "UTF-8");
        let _ = std::fs::remove_file(&tmp);
    }

    // ---- tail-read memory bound and backlog continuity ----------------------
    //
    // These guard the invariants that keep a large log append from turning into an
    // unbounded (or aborting) allocation: one call never buffers more than its
    // budget, the backlog still drains completely and in order, and an over-long
    // line neither spins nor loses bytes.

    /// Drain `rx` on a helper thread so a bounded channel cannot deadlock the
    /// reader under test; returns the received lines in order.
    fn collect_lines(
        rx: crossbeam_channel::Receiver<(u64, String)>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Ok((_, line)) = rx.recv() {
                got.push(line);
            }
            got
        })
    }

    #[test]
    fn tail_chunked_reads_cover_the_backlog_in_order_without_loss() {
        let tmp = unique_tmp("tail_chunked");
        let expected: Vec<String> = (0..200).map(|i| format!("line-{i:04}")).collect();
        {
            let mut f = File::create(&tmp).unwrap();
            for line in &expected {
                writeln!(f, "{line}").unwrap();
            }
        }
        let len = std::fs::metadata(&tmp).unwrap().len();

        // A 64-byte budget over ~2 KiB forces many chunks; the small bounded
        // channel also makes the reader hit backpressure mid-chunk.
        let (tx, rx) = crossbeam_channel::bounded(4);
        let collector = collect_lines(rx);

        let mut offset = 0u64;
        let mut calls = 0;
        loop {
            let a = read_appended_with_budget(&tmp, offset, encoding_rs::UTF_8, &tx, 1, None, 64)
                .unwrap();
            calls += 1;
            assert!(a.offset > offset, "every call must make progress");
            assert!(
                a.offset - offset <= 64,
                "buffered {} bytes with a 64-byte budget",
                a.offset - offset
            );
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(calls < 1000, "backlog never drained");
        }
        assert_eq!(offset, len, "all complete lines consumed");
        assert!(calls > 1, "expected several chunks, got {calls}");

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(
            got, expected,
            "no line may be lost, duplicated or reordered"
        );
    }

    #[test]
    fn tail_overlong_line_advances_without_spinning_or_truncating() {
        let tmp = unique_tmp("tail_overlong");
        let original = vec![b'z'; 250];
        std::fs::write(&tmp, &original).unwrap();

        let (tx, rx) = crossbeam_channel::bounded(8);
        let collector = collect_lines(rx);

        // 100-byte budget, 250 bytes with no newline anywhere.
        let mut offset = 0u64;
        let mut calls = 0;
        loop {
            let a = read_appended_with_budget(&tmp, offset, encoding_rs::UTF_8, &tx, 1, None, 100)
                .unwrap();
            calls += 1;
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(calls < 100, "an over-long line must not spin");
        }
        // Two budget-sized chunks are emitted; the trailing partial chunk is
        // withheld (the reader still cannot see a complete line).
        assert_eq!(offset, 200, "must advance by the budget, not stall at 0");
        assert_eq!(
            calls, 3,
            "the third call must report no progress and no more"
        );

        // Completing the line releases the withheld remainder intact.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(b"\n").unwrap();
        }
        let a =
            read_appended_with_budget(&tmp, offset, encoding_rs::UTF_8, &tx, 1, None, 100).unwrap();
        assert_eq!(
            a.offset,
            original.len() as u64 + 1,
            "rest of the line consumed"
        );
        assert!(!a.more);

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        let joined: String = got.concat();
        assert_eq!(
            joined.as_bytes(),
            original.as_slice(),
            "no byte of the over-long line may be lost"
        );
    }

    #[test]
    fn tail_force_split_lands_on_utf8_char_boundaries() {
        let tmp = unique_tmp("tail_utf8_boundary");
        let text = "中".repeat(12);
        std::fs::write(&tmp, format!("{text}\n").as_bytes()).unwrap();
        let len = std::fs::metadata(&tmp).unwrap().len();

        let (tx, rx) = crossbeam_channel::bounded(8);
        let collector = collect_lines(rx);

        // A 10-byte budget over 3-byte characters: every force-split would land
        // inside a character if the reader did not back off to a boundary.
        let mut offset = 0u64;
        let mut guard = 0;
        loop {
            let a = read_appended_with_budget(&tmp, offset, encoding_rs::UTF_8, &tx, 1, None, 10)
                .unwrap();
            guard += 1;
            assert!(a.offset > offset, "a force-split must still advance");
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(guard < 100, "must not spin");
        }
        assert_eq!(offset, len, "the whole line must be consumed");

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        let joined: String = got.concat();
        assert!(
            !joined.contains('\u{fffd}'),
            "splitting on a character boundary must not produce replacement characters"
        );
        assert_eq!(
            joined, text,
            "the unconsumed tail must survive to the next read"
        );
    }

    #[test]
    fn tail_budget_smaller_than_one_character_still_advances() {
        let tmp = unique_tmp("tail_tiny_budget");
        let content = "中中中\n"; // 10 bytes; every character is 3 bytes
        std::fs::write(&tmp, content.as_bytes()).unwrap();
        let len = std::fs::metadata(&tmp).unwrap().len();

        let (tx, rx) = crossbeam_channel::bounded(8);
        let collector = collect_lines(rx);

        // A 1-byte budget can never hold a complete character, so backing off to
        // a boundary would consume nothing. Each call must still make progress
        // (lossily) instead of stalling or spinning.
        let mut offset = 0u64;
        let mut calls = 0usize;
        loop {
            let a = read_appended_with_budget(&tmp, offset, encoding_rs::UTF_8, &tx, 1, None, 1)
                .unwrap();
            calls += 1;
            assert!(
                a.offset > offset || !a.more,
                "reporting `more` while consuming nothing is what spins the follower"
            );
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(calls <= len as usize + 1, "must not spin: {calls} calls");
        }
        assert_eq!(offset, len, "every byte must eventually be consumed");

        drop(tx);
        let _ = collector.join();
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn tail_force_split_keeps_utf16_surrogate_pairs_whole() {
        let tmp = unique_tmp("tail_utf16_surrogate");
        let text = "😀".repeat(3);
        let mut bytes: Vec<u8> = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0x0Au16.to_le_bytes());
        std::fs::write(&tmp, &bytes).unwrap();
        let len = bytes.len() as u64;

        let (tx, rx) = crossbeam_channel::bounded(8);
        let collector = collect_lines(rx);

        // 6 bytes = 3 code units, which always lands between the halves of a
        // surrogate pair; the high half must be held back for the next read
        // rather than decoded as U+FFFD on this one.
        let mut offset = 0u64;
        let mut guard = 0;
        loop {
            let a = read_appended_with_budget(&tmp, offset, encoding_rs::UTF_16LE, &tx, 1, None, 6)
                .unwrap();
            guard += 1;
            assert!(a.offset > offset, "a force-split must still advance");
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(guard < 100, "must not spin");
        }
        assert_eq!(offset, len, "the whole line must be consumed");

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        let joined: String = got.concat();
        assert!(
            !joined.contains('\u{fffd}'),
            "splitting between surrogate halves must not produce replacement characters"
        );
        assert_eq!(joined, text);
    }

    #[test]
    fn tail_reserve_failure_returns_an_error_instead_of_aborting() {
        // `usize::MAX` cannot be a capacity, so this fails before touching the
        // allocator. What matters is that it comes back as `Err` rather than
        // reaching `handle_alloc_error` (which aborts) and that no buffer is
        // handed out for the caller to read or write unprepared memory.
        let err = reserve_buffer(usize::MAX).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::OutOfMemory);
        // The failure must also carry our marker, because that — not the kind —
        // is what tells the follower to stop instead of retrying.
        assert!(
            err.get_ref()
                .is_some_and(|inner| inner.is::<PermanentTailReadError>()),
            "reservation failures must be recognisable as deliberately permanent"
        );
    }

    #[test]
    fn tail_truncation_and_replacement_after_a_partial_backlog_are_detected() {
        let tmp = unique_tmp("tail_backlog_rotate");
        let backup = unique_tmp("tail_backlog_backup");
        let content: String = (0..100).map(|i| format!("row-{i:04}\n")).collect();
        std::fs::write(&tmp, &content).unwrap();

        let (tx, rx) = crossbeam_channel::bounded(4);
        let collector = collect_lines(rx);
        let identity = file_identity(&File::open(&tmp).unwrap()).unwrap();

        // Consume one budget-sized chunk: the offset is now mid-file and a
        // backlog is still pending.
        let first =
            read_appended_with_budget(&tmp, 0, encoding_rs::UTF_8, &tx, 1, Some(identity), 64)
                .unwrap();
        assert!(first.more, "a backlog must still be pending");
        assert!(first.offset > 0 && first.offset < content.len() as u64);

        // Truncation below the consumed offset must ask for a reload instead of
        // appending at a now-invalid offset.
        std::fs::write(&tmp, b"tiny\n").unwrap();
        let after_trunc = read_appended_with_budget(
            &tmp,
            first.offset,
            encoding_rs::UTF_8,
            &tx,
            1,
            Some(identity),
            64,
        )
        .unwrap();
        assert!(after_trunc.truncated, "truncation must trigger a reload");
        assert_eq!(after_trunc.offset, first.offset, "offset must not move");

        // A replacement file (new inode) must also be detected, even when it is
        // longer than the offset so the length check alone would pass.
        std::fs::rename(&tmp, &backup).unwrap();
        let replacement = format!("new header\n{}\n", "y".repeat(first.offset as usize + 8));
        std::fs::write(&tmp, replacement).unwrap();
        let after_replace = read_appended_with_budget(
            &tmp,
            first.offset,
            encoding_rs::UTF_8,
            &tx,
            1,
            Some(identity),
            64,
        )
        .unwrap();
        assert!(after_replace.truncated, "replacement must trigger a reload");

        drop(tx);
        let _ = collector.join();
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&backup);
    }

    /// Write `count` deterministic lines of `line_len` bytes each (including the
    /// newline) and return the file length. The index is baked into every line so
    /// a duplicated, dropped or reordered row is detectable.
    fn write_indexed_lines(path: &std::path::Path, count: u64, line_len: usize) -> u64 {
        let payload = "p".repeat(line_len - 17); // 16-byte index + payload + '\n'
        let mut f = std::io::BufWriter::with_capacity(1 << 20, File::create(path).unwrap());
        for i in 0..count {
            writeln!(f, "{i:016}{payload}").unwrap();
        }
        f.flush().unwrap();
        std::fs::metadata(path).unwrap().len()
    }

    #[test]
    fn tail_160mib_backlog_is_consumed_completely_without_loss_or_duplication() {
        // The production entry point with the real MAX_TAIL_READ: a backlog far
        // larger than one poll must arrive complete, in order, exactly once.
        const TARGET: u64 = 160 * 1024 * 1024;
        const LINE_LEN: usize = 4096;
        let tmp = unique_tmp("tail_backlog_160mib");
        let count = TARGET / LINE_LEN as u64;
        let len = write_indexed_lines(&tmp, count, LINE_LEN);

        let (tx, rx) = crossbeam_channel::bounded(8192);
        let collector = collect_lines(rx);

        let mut offset = 0u64;
        let mut calls = 0;
        loop {
            let a = read_appended(&tmp, offset, encoding_rs::UTF_8, &tx, 1).unwrap();
            calls += 1;
            assert!(a.offset > offset, "must progress");
            assert!(
                a.offset - offset <= MAX_TAIL_READ,
                "one poll buffered {} bytes, over MAX_TAIL_READ",
                a.offset - offset
            );
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(calls < 100, "backlog never drained");
        }
        assert_eq!(offset, len, "the backlog must be consumed to EOF");
        assert!(
            calls >= 3,
            "160 MiB needs >= 3 polls of 64 MiB, took {calls}"
        );

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(got.len() as u64, count, "line count must match exactly");
        for (i, line) in got.iter().enumerate() {
            assert_eq!(line.len(), LINE_LEN - 1, "line {i} has the wrong length");
            assert_eq!(
                &line[..16],
                format!("{i:016}").as_str(),
                "line {i} is out of order or duplicated"
            );
        }
        let bytes: u64 = got.iter().map(|l| l.len() as u64 + 1).sum();
        assert_eq!(bytes, len, "no byte may be lost or duplicated");
    }

    #[test]
    fn tail_100mib_line_without_newline_keeps_advancing() {
        const TOTAL: usize = 100 * 1024 * 1024;
        let tmp = unique_tmp("tail_100mib_nonewline");
        {
            let mut f = std::io::BufWriter::with_capacity(1 << 20, File::create(&tmp).unwrap());
            let block = vec![b'x'; 1 << 20];
            for _ in 0..(TOTAL >> 20) {
                f.write_all(&block).unwrap();
            }
            f.flush().unwrap();
        }
        let len = std::fs::metadata(&tmp).unwrap().len();
        assert_eq!(len as usize, TOTAL);

        let (tx, rx) = crossbeam_channel::bounded(4);
        let collector = collect_lines(rx);

        // Poll 1: exactly one budget-sized chunk of the un-terminated line.
        let first = read_appended(&tmp, 0, encoding_rs::UTF_8, &tx, 1).unwrap();
        assert_eq!(
            first.offset, MAX_TAIL_READ,
            "must advance by the budget, not stall at 0"
        );
        assert!(first.more, "the rest of the line is still pending");

        // Poll 2: the remainder is still un-terminated with nothing beyond it, so
        // it is held back — but it must report no `more`, or the caller would spin.
        let second = read_appended(&tmp, first.offset, encoding_rs::UTF_8, &tx, 1).unwrap();
        assert_eq!(
            second.offset, first.offset,
            "an unterminated tail is left for the next read"
        );
        assert!(!second.more, "no progress must never be reported as `more`");

        // Completing the line releases the remainder intact.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(b"\n").unwrap();
        }
        let third = read_appended(&tmp, second.offset, encoding_rs::UTF_8, &tx, 1).unwrap();
        assert_eq!(third.offset, len + 1, "the whole line must be consumed");
        assert!(!third.more);

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        let joined: String = got.concat();
        assert_eq!(joined.len(), TOTAL, "every byte must survive");
        assert!(
            joined.bytes().all(|b| b == b'x'),
            "content must be preserved exactly"
        );
    }

    #[test]
    fn tail_utf8_multibyte_char_straddling_the_budget_boundary_stays_intact() {
        let tmp = unique_tmp("tail_straddle");
        let budget = MAX_TAIL_READ as usize;
        // Start the emoji two bytes before the boundary, so the first poll reads
        // into it and would split it without the character-boundary back-off.
        let prefix = budget - 2;
        let expected = format!("{}😀中", "a".repeat(prefix));

        let (tx, rx) = crossbeam_channel::bounded(4);
        let collector = collect_lines(rx);
        {
            let mut f = std::io::BufWriter::with_capacity(1 << 20, File::create(&tmp).unwrap());
            f.write_all(expected.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
            f.flush().unwrap();
        }
        let len = std::fs::metadata(&tmp).unwrap().len();

        let mut offset = 0u64;
        let mut calls = 0;
        loop {
            let a = read_appended(&tmp, offset, encoding_rs::UTF_8, &tx, 1).unwrap();
            calls += 1;
            assert!(a.offset > offset, "must progress");
            offset = a.offset;
            if !a.more {
                break;
            }
            assert!(calls < 10, "must not spin");
        }
        assert_eq!(offset, len);

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        let joined: String = got.concat();
        assert!(
            calls >= 2,
            "the straddle must force a second poll, got {calls}"
        );
        assert!(
            !joined.contains('\u{fffd}'),
            "a character straddling the 64 MiB boundary must not be split"
        );
        assert_eq!(joined, expected, "content must survive the boundary intact");
    }

    #[test]
    fn tail_utf16_offsets_are_correct_at_chunk_boundaries_and_newlines() {
        let tmp = unique_tmp("tail_utf16_offsets");
        let budget = MAX_TAIL_READ as usize; // even, so a whole number of code units
                                             // A first line whose terminating newline sits exactly on the budget
                                             // boundary, followed by a surrogate pair that the next chunk must decode.
        let first_line = "a".repeat(budget / 2 - 1);
        let mut bytes: Vec<u8> = Vec::with_capacity(budget + 8);
        for unit in first_line.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0x0Au16.to_le_bytes());
        for unit in "😀b\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&tmp, &bytes).unwrap();
        let len = bytes.len() as u64;
        drop(bytes);

        let (tx, rx) = crossbeam_channel::bounded(4);
        let collector = collect_lines(rx);

        // Poll 1 consumes the newline that sits exactly on the boundary, and not
        // a byte more.
        let a = read_appended(&tmp, 0, encoding_rs::UTF_16LE, &tx, 1).unwrap();
        assert_eq!(
            a.offset, MAX_TAIL_READ,
            "the newline on the boundary must be consumed exactly"
        );
        assert!(a.more, "the following line is still pending");

        // Poll 2 starts on a whole code unit and consumes the rest to EOF.
        let b = read_appended(&tmp, a.offset, encoding_rs::UTF_16LE, &tx, 1).unwrap();
        assert_eq!(b.offset, len, "the tail must be consumed exactly to EOF");
        assert!(!b.more);

        drop(tx);
        let got = collector.join().unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(
            got,
            vec![first_line, "😀b".to_string()],
            "offsets and content must survive the boundary"
        );
    }

    #[test]
    fn tail_send_failure_reports_no_progress_instead_of_looping() {
        let tmp = unique_tmp("tail_send_closed");
        std::fs::write(&tmp, b"a\nb\nc\n").unwrap();

        // Receiver gone: every `send` fails immediately.
        let (tx, rx) = crossbeam_channel::bounded::<(u64, String)>(1);
        drop(rx);

        let start = std::time::Instant::now();
        for _ in 0..10 {
            let a = read_appended(&tmp, 0, encoding_rs::UTF_8, &tx, 1).unwrap();
            assert_eq!(a.offset, 0, "nothing was delivered, so nothing is consumed");
            assert!(
                !a.more,
                "a closed channel must not look like a pending backlog"
            );
        }
        let _ = std::fs::remove_file(&tmp);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "a closed channel must not block or spin"
        );
    }

    #[test]
    fn file_sends_apply_backpressure_instead_of_queueing_the_whole_file() {
        // The channel is unbounded so the device reader never blocks, which means a
        // file reader has to limit itself: without this it would queue the entire
        // file as Strings ahead of the slower parse/append step.
        let (tx, rx) = crossbeam_channel::unbounded::<(u64, String)>();
        let sender = std::thread::spawn(move || {
            let mut sent = 0usize;
            while sent < 100_000 && send_throttled(&tx, 1, String::from("line")) {
                sent += 1;
            }
            sent
        });

        // Nobody consumes: it must park near the limit rather than queue all 100k.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            rx.len() <= MAX_QUEUED_LINES + 1,
            "queued {} lines with nobody consuming",
            rx.len()
        );

        // Consuming must let it continue.
        let mut drained = 0usize;
        while drained < MAX_QUEUED_LINES * 2 {
            match rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(_) => drained += 1,
                Err(_) => break,
            }
        }
        assert!(
            drained >= MAX_QUEUED_LINES,
            "draining must unblock the file reader, only {drained} came through"
        );
        drop(rx);
        let _ = sender.join();
    }
}
