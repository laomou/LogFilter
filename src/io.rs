use crate::model::EncodingChoice;
use crossbeam_channel::Sender;
use encoding_rs::{CoderResult, Decoder, Encoding};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How the tail/follow poller should pick up data appended after the initial
/// load, decided once from the encoding chosen for that load.
pub enum Tail {
    /// `offset` is the byte position just past the end of the initial load. For
    /// UTF-8 and byte-safe legacy codepages (GBK, Shift_JIS, EUC-KR, Big5,
    /// Windows-1252) appended data is split on raw `\n`; UTF-16 is decoded into
    /// code units and split on the '\n' character (see `read_appended`).
    Append { offset: u64, enc: &'static Encoding },
}

impl Tail {
    /// Canonical name of the encoding actually used for this load (e.g. "UTF-8",
    /// "GBK", "UTF-16LE"). Lets the UI show the detected encoding rather than the
    /// user's menu choice — notably "Local" resolves to whatever was sniffed.
    pub fn encoding_name(&self) -> &'static str {
        match self {
            Tail::Append { enc, .. } => enc.name(),
        }
    }
}

/// Result of reading data appended since a previous tail offset.
pub struct Appended {
    /// New byte offset (just past the last complete line consumed).
    pub offset: u64,
    /// True if the file is now shorter than `offset` — rotated or truncated,
    /// so the caller should reload from scratch rather than append.
    pub truncated: bool,
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
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(Tail::Append {
                offset: read_bytes,
                enc: encoding_rs::UTF_8,
            });
        }
        read_bytes += n as u64;
        if source_epoch.load(Ordering::Acquire) != epoch {
            return Ok(Tail::Append {
                offset: read_bytes,
                enc: encoding_rs::UTF_8,
            });
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
        if tx.send((epoch, line)).is_err() {
            return Ok(Tail::Append {
                offset: read_bytes,
                enc: encoding_rs::UTF_8,
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
        Tail::Append { offset, enc }
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
    if !text_buf.is_empty() {
        if source_epoch.load(Ordering::Acquire) != epoch {
            return Ok(tail_for(read_bytes));
        }
        let line = text_buf.trim_end_matches(['\r', '\n']).to_string();
        // Match the UTF-8 reader's `read_until` behavior: a final logical line
        // is preserved even if CR/LF normalization leaves it empty (for example
        // a file ending in a lone `\r`).
        if tx.send((epoch, line)).is_err() {
            return Ok(tail_for(read_bytes));
        }
    }
    Ok(tail_for(read_bytes))
}

/// Read bytes appended to `path` since `offset` and send each COMPLETE line
/// (terminated by `\n`) via `tx`, decoded with `enc`. A trailing partial line
/// (no newline yet — a log entry mid-write) is intentionally left unread so it
/// isn't shown truncated; it will be picked up once its newline arrives. This
/// is the "strategy A" tailing behavior.
///
/// Returns the new offset (advanced only past complete lines) and whether the
/// file is now shorter than `offset` (rotated/truncated → caller should reload).
pub fn read_appended(
    path: &Path,
    offset: u64,
    enc: &'static Encoding,
    tx: &Sender<(u64, String)>,
    epoch: u64,
) -> std::io::Result<Appended> {
    let mut file = File::open(path)?;
    let len = file.seek(SeekFrom::End(0))?;
    if len < offset {
        return Ok(Appended {
            offset,
            truncated: true,
        });
    }
    if len == offset {
        return Ok(Appended {
            offset,
            truncated: false,
        });
    }
    file.seek(SeekFrom::Start(offset))?;
    // Read everything appended so far into memory (bounded by how much the log
    // grew between 500 ms polls — small in practice).
    let mut raw = Vec::with_capacity((len - offset) as usize);
    file.read_to_end(&mut raw)?;

    // UTF-16: `0x0A` also occurs as a byte inside unrelated code units, so we
    // can't split on raw newline bytes. Decode whole 2-byte code units, split on
    // the '\n' *character*, and advance the offset by the UTF-16 byte length of
    // what we consumed. (This lets UTF-16 files tail incrementally instead of
    // reloading the whole file on every size change.)
    if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
        let usable = raw.len() & !1; // whole code units only; hold back a stray byte
                                     // decode_without_bom_handling: a plain decode() would BOM-sniff the chunk
                                     // and, if it happens to start with BOM-like bytes (a real U+FEFF char, or
                                     // 0xFFFE), drop a char or switch encoding — drifting the byte offset.
        let (text, _) = enc.decode_without_bom_handling(&raw[..usable]);
        let Some(nl) = text.rfind('\n') else {
            return Ok(Appended {
                offset,
                truncated: false,
            });
        };
        let consumed = &text[..=nl];
        for chunk in consumed.split_inclusive('\n') {
            let line = chunk.trim_end_matches(['\r', '\n']).to_string();
            if tx.send((epoch, line)).is_err() {
                break;
            }
        }
        let consumed_bytes = consumed.encode_utf16().count() as u64 * 2;
        return Ok(Appended {
            offset: offset + consumed_bytes,
            truncated: false,
        });
    }

    // Only consume up to the last newline; bytes after it are a partial line.
    let last_nl = raw.iter().rposition(|&b| b == b'\n');
    let Some(end) = last_nl else {
        // No complete line yet — leave the offset untouched.
        return Ok(Appended {
            offset,
            truncated: false,
        });
    };
    let complete = &raw[..=end];
    for chunk in complete.split_inclusive(|&b| b == b'\n') {
        let (cow, _, _) = enc.decode(chunk);
        let line = cow.trim_end_matches(['\r', '\n']).to_string();
        if tx.send((epoch, line)).is_err() {
            break;
        }
    }
    Ok(Appended {
        offset: offset + complete.len() as u64,
        truncated: false,
    })
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
            if tx.send((epoch, line.to_string())).is_err() {
                return consumed;
            }
            consumed = scan + 1;
        }
        scan += 1;
    }
    consumed
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
            Tail::Append { offset, enc } => {
                assert_eq!(offset, 12);
                assert_eq!(enc.name(), "UTF-8");
            }
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn tail_encoding_name_reports_actual_encoding() {
        assert_eq!(
            Tail::Append {
                offset: 0,
                enc: encoding_rs::GBK
            }
            .encoding_name(),
            "GBK"
        );
        assert_eq!(
            Tail::Append {
                offset: 0,
                enc: encoding_rs::UTF_16LE
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
}
