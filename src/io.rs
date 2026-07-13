use crate::model::EncodingChoice;
use crossbeam_channel::Sender;
use encoding_rs::Encoding;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub fn send_utf8_lines(
    path: &Path,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
) {
    let Ok(file) = std::fs::File::open(path) else { return; };
    let mut reader = BufReader::new(file);
    let bom = match reader.fill_buf() {
        Ok(buf) => buf,
        Err(_) => return,
    };
    if bom.starts_with(&[0xFF, 0xFE]) || bom.starts_with(&[0xFE, 0xFF]) {
        drop(reader);
        send_decoded_lines(path, tx, epoch, source_epoch, EncodingChoice::Utf8);
        return;
    }

    let mut buf = Vec::new();
    let mut first = true;
    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else { return; };
        if n == 0 { return; }
        if source_epoch.load(Ordering::Acquire) != epoch { return; }
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
        if tx.send((epoch, line)).is_err() { return; }
    }
}

pub fn send_decoded_lines(
    path: &Path,
    tx: Sender<(u64, String)>,
    epoch: u64,
    source_epoch: Arc<AtomicU64>,
    choice: EncodingChoice,
) {
    let enc = match choice {
        EncodingChoice::Utf8 => encoding_rs::UTF_8,
        EncodingChoice::Local => {
            let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
            pick_local_encoding(&locale)
        }
    };
    let Ok(file) = std::fs::File::open(path) else { return; };
    let mut reader = BufReader::with_capacity(8192, file);
    let mut decoder = enc.new_decoder();
    // Accumulate decoded text across chunks so we can split into lines only when
    // a full line is available — avoids splitting a multibyte character mid-sequence.
    let mut text_buf = String::with_capacity(8192);
    let mut raw_buf = vec![0u8; 8192];
    loop {
        use std::io::Read;
        let Ok(n) = reader.read(&mut raw_buf) else { return; };
        if n == 0 { break; }
        if source_epoch.load(Ordering::Acquire) != epoch { return; }
        let _ = decoder.decode_to_string(&raw_buf[..n], &mut text_buf, false);
        // Flush complete lines while keeping any partial trailing line in `text_buf`.
        {
            // Find the last newline to keep the trailing partial line.
            let mut drain_end = 0usize;
            let mut bytes = text_buf.as_bytes();
            while drain_end < bytes.len() {
                if bytes[drain_end] == b'\n' {
                    let line = text_buf[..drain_end].trim_end_matches(['\r', '\n']);
                    if tx.send((epoch, line.to_string())).is_err() { return; }
                    let next = drain_end + 1;
                    // Drain the emitted portion so we don't re-scan on next chunk.
                    text_buf.drain(..next);
                    drain_end = 0;
                    bytes = text_buf.as_bytes(); // refresh after drain
                    if bytes.is_empty() { break; }
                } else {
                    drain_end += 1;
                }
            }
        }
        if source_epoch.load(Ordering::Acquire) != epoch { return; }
    }
    // Final flush: drain any remaining buffered bytes from the decoder.
    let _ = decoder.decode_to_string(b"", &mut text_buf, true);
    // Emit any remaining text without a trailing newline as a final line.
    if !text_buf.is_empty() {
        if source_epoch.load(Ordering::Acquire) != epoch { return; }
        let line = text_buf.trim_end_matches(['\r', '\n']).to_string();
        if !line.is_empty() {
            let _ = tx.send((epoch, line));
        }
    }
}

pub fn pick_local_encoding(locale: &str) -> &'static Encoding {
    let low = locale.to_lowercase();
    if low.starts_with("zh") { encoding_rs::GBK }
    else if low.starts_with("ja") { encoding_rs::SHIFT_JIS }
    else if low.starts_with("ko") { encoding_rs::EUC_KR }
    else { encoding_rs::WINDOWS_1252 }
}
