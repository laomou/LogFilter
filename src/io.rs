use crate::model::EncodingChoice;
use crossbeam_channel::Sender;
use encoding_rs::Encoding;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Stream a UTF-8 (or BOM-detected UTF-16) file's lines into `tx`.
///
/// Returns `Ok(())` on clean end-of-file or when a newer source epoch
/// supersedes this load; returns `Err` only on a genuine mid-stream read
/// failure, so the caller can distinguish "finished" from "truncated by an I/O
/// error" and surface the latter instead of presenting a partial file as whole.
pub fn send_utf8_lines(
    file: File,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(file);
    let bom = reader.fill_buf()?;
    if bom.starts_with(&[0xFF, 0xFE]) || bom.starts_with(&[0xFE, 0xFF]) {
        // UTF-16 BOM detected: pick LE/BE and delegate to the decoded path.
        let is_le = bom.starts_with(&[0xFF, 0xFE]);
        let file2 = reader.into_inner();
        let enc = if is_le { encoding_rs::UTF_16LE } else { encoding_rs::UTF_16BE };
        return send_decoded_lines_with_enc(file2, tx, epoch, source_epoch, enc);
    }

    let mut buf = Vec::new();
    let mut first = true;
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 { return Ok(()); }
        if source_epoch.load(Ordering::Acquire) != epoch { return Ok(()); }
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
                if line.starts_with('\u{feff}') { line.remove(0); }
            }
            while line.ends_with(['\n', '\r']) { line.pop(); }
            line
        };
        if tx.send((epoch, line)).is_err() { return Ok(()); }
    }
}

pub fn send_decoded_lines(
    file: File,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    choice: EncodingChoice,
) -> std::io::Result<()> {
    let enc = match choice {
        EncodingChoice::Utf8 => encoding_rs::UTF_8,
        EncodingChoice::Local => {
            let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
            pick_local_encoding(&locale)
        }
    };
    send_decoded_lines_with_enc(file, tx, epoch, source_epoch, enc)
}

fn send_decoded_lines_with_enc(
    file: File,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    enc: &'static Encoding,
) -> std::io::Result<()> {
    let mut reader = BufReader::with_capacity(8192, file);
    let mut decoder = enc.new_decoder();
    // Accumulate decoded text across chunks so we can split into lines only when
    // a full line is available — avoids splitting a multibyte character mid-sequence.
    let mut text_buf = String::with_capacity(8192);
    let mut raw_buf = vec![0u8; 8192];
    loop {
        use std::io::Read;
        let n = reader.read(&mut raw_buf)?;
        if n == 0 { break; }
        if source_epoch.load(Ordering::Acquire) != epoch { return Ok(()); }
        let _ = decoder.decode_to_string(&raw_buf[..n], &mut text_buf, false);
        // Flush complete lines in one pass, then drain once per chunk (O(n)).
        let consumed = flush_lines(&text_buf, &tx, epoch);
        if consumed > 0 { text_buf.drain(..consumed); }
        if source_epoch.load(Ordering::Acquire) != epoch { return Ok(()); }
    }
    // Final flush: drain any remaining buffered bytes from the decoder.
    let _ = decoder.decode_to_string(b"", &mut text_buf, true);
    // Emit any remaining text without a trailing newline as a final line.
    if !text_buf.is_empty() {
        if source_epoch.load(Ordering::Acquire) != epoch { return Ok(()); }
        let line = text_buf.trim_end_matches(['\r', '\n']).to_string();
        if !line.is_empty() {
            let _ = tx.send((epoch, line));
        }
    }
    Ok(())
}

/// Scan `buf` for complete lines (terminated by `\n`), send each via `tx`,
/// and return the number of bytes consumed (the processed prefix length).
/// The caller drains that prefix once — O(n) per chunk instead of O(n²).
fn flush_lines(
    buf: &str,
    tx: &Sender<(u64, String)>,
    epoch: u64,
) -> usize {
    let bytes = buf.as_bytes();
    let mut consumed = 0usize;
    let mut scan = 0usize;
    while scan < bytes.len() {
        if bytes[scan] == b'\n' {
            let line = buf[consumed..scan].trim_end_matches(['\r', '\n']);
            if tx.send((epoch, line.to_string())).is_err() { return consumed; }
            consumed = scan + 1;
        }
        scan += 1;
    }
    consumed
}

pub fn pick_local_encoding(locale: &str) -> &'static Encoding {
    let low = locale.to_lowercase();
    if low.starts_with("zh") { encoding_rs::GBK }
    else if low.starts_with("ja") { encoding_rs::SHIFT_JIS }
    else if low.starts_with("ko") { encoding_rs::EUC_KR }
    else { encoding_rs::WINDOWS_1252 }
}
