use anyhow::{anyhow, Result};
use crossbeam_channel::Sender;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Windows CREATE_NO_WINDOW: suppresses the black cmd.exe window that would
/// otherwise flash for every child process when running as a GUI (windows
/// subsystem) executable.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Build a Command for `adb` that suppresses console popups on Windows.
fn adb_command(override_path: Option<&str>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(adb_binary(override_path));
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Locate the `adb` executable. If the user set an explicit path in config,
/// use that. Otherwise return the bare name and let `std::process::Command`
/// resolve it against PATH (Rust searches PATHEXT on Windows, so `adb`,
/// `adb.exe`, and `adb.bat` are all handled). Windows-only fallback: the
/// Android Studio default install location.
pub fn adb_binary(override_path: Option<&str>) -> PathBuf {
    if let Some(p) = override_path {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let candidate = PathBuf::from(local).join("Android/Sdk/platform-tools/adb.exe");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("adb")
}

pub fn list_devices(adb_override: Option<&str>) -> Result<Vec<String>> {
    let out = adb_command(adb_override)
        .arg("devices")
        .output()
        .map_err(|e| anyhow!("failed to spawn `adb devices`: {} (is adb on PATH?)", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "`adb devices` exited with {:?}: {}",
            out.status.code(),
            stderr.trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut devices = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip informational lines that adb sometimes prints alongside real entries.
        if line.starts_with("List of devices") {
            continue;
        }
        if line.starts_with('*') {
            continue;
        }
        // Real entries look like "SERIAL\tdevice" or "SERIAL\toffline".
        if let Some((serial, state)) = line.split_once(|c: char| c.is_whitespace()) {
            let state = state.trim();
            if serial.is_empty() {
                continue;
            }
            // Keep all reachable entries; offline devices can still be shown so
            // the user knows about them, but drop unauthorized/permission ones.
            if state == "unauthorized" || state == "no permissions" {
                continue;
            }
            devices.push(serial.to_string());
        }
    }
    Ok(devices)
}

pub struct Session {
    child: Child,
    paused: Arc<std::sync::atomic::AtomicBool>,
    reader_thread: Option<thread::Thread>,
    reader_handle: Option<thread::JoinHandle<()>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
    stopped: bool,
    /// Set true when the adb stdout closes (process exited / stream ended), so
    /// the UI can detect a session that died on its own and stop showing it as
    /// live. Distinct from `stop()`, which the user initiated.
    ended: Arc<AtomicBool>,
    /// Captured stderr from the adb child. Populated by a reader thread; read by
    /// the UI when the session ends to surface the reason (device offline,
    /// unknown command, etc.) instead of a silent empty stream.
    stderr: Arc<Mutex<String>>,
}

impl Session {
    /// Spawn `adb [-s serial] <cmd_args...>` and stream stdout lines into `tx`,
    /// each tagged with `epoch` so the ingest side can drop lines from a
    /// superseded source. Blank lines are preserved; the reader thread exits
    /// when stdout closes.
    pub fn start(
        adb_override: Option<&str>,
        device: Option<&str>,
        cmd: &str,
        tx: Sender<(u64, String)>,
        epoch: u64,
    ) -> Result<Self> {
        let mut command = adb_command(adb_override);
        if let Some(d) = device {
            command.arg("-s").arg(d);
        }
        // Split the command string respecting shell quoting (e.g. `logcat -s "My Tag"`).
        // Falls back to simple whitespace split if the string is not valid shell syntax.
        let args = shlex::split(cmd)
            .unwrap_or_else(|| cmd.split_whitespace().map(str::to_string).collect());
        for tok in args {
            command.arg(tok);
        }
        // Capture stderr so device/command errors are visible instead of being
        // inherited into the parent's console (invisible in a GUI build).
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("failed to spawn adb: {} (is adb on PATH?)", e))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused_thr = paused.clone();
        let ended = Arc::new(AtomicBool::new(false));
        let ended_thr = ended.clone();
        let handle = thread::Builder::new()
            .name("adb-reader".into())
            .spawn(move || {
                // Cap on lines held while paused: keeps memory bounded on a long
                // pause under a heavy log rate (~a few MB at this size).
                const PAUSE_BUFFER_CAP: usize = 100_000;
                let reader = BufReader::new(stdout);
                let mut buffer = std::collections::VecDeque::new();
                stream_lossy_lines(reader, |line| {
                    // While paused, keep reading (drain the kernel pipe so adb
                    // doesn't back up and logd doesn't rotate our logs away) but
                    // hold lines in a bounded buffer; flush them on resume.
                    let paused = paused_thr.load(std::sync::atomic::Ordering::Relaxed);
                    route_line(paused, line, &mut buffer, PAUSE_BUFFER_CAP, |l| {
                        tx.send((epoch, l)).is_ok()
                    })
                });
                // stdout closed → the adb process ended (or was killed). Signal the
                // UI so it doesn't keep showing a live session that emits nothing.
                ended_thr.store(true, Ordering::Release);
            })?;
        let reader_thread = handle.thread().clone();

        // Drain stderr on its own thread into a shared buffer.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_handle = if let Some(mut es) = child.stderr.take() {
            let buf = stderr_buf.clone();
            Some(
                thread::Builder::new()
                    .name("adb-stderr".into())
                    .spawn(move || {
                        let mut s = String::new();
                        if es.read_to_string(&mut s).is_ok() && !s.is_empty() {
                            if let Ok(mut guard) = buf.lock() {
                                guard.push_str(&s);
                            }
                        }
                    })?,
            )
        } else {
            None
        };

        Ok(Self {
            child,
            paused,
            reader_thread: Some(reader_thread),
            reader_handle: Some(handle),
            stderr_handle,
            stopped: false,
            ended,
            stderr: stderr_buf,
        })
    }

    /// True once the adb process's stdout has closed on its own (not via a
    /// user-initiated `stop()`).
    pub fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Snapshot of any stderr captured from the adb child so far.
    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|g| g.trim().to_string())
            .unwrap_or_default()
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, std::sync::atomic::Ordering::Relaxed);
        if !p {
            // Wake the reader thread; if it was parked, it resumes reading.
            if let Some(t) = &self.reader_thread {
                t.unpark();
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = self.child.kill();
        // Wake the reader so it can see the broken pipe / empty read and exit.
        self.paused.store(false, Ordering::Relaxed);
        if let Some(t) = &self.reader_thread {
            t.unpark();
        }
        let _ = self.child.wait();
        self.join_workers();
    }

    /// Reap a session that ended on its own (its stdout closed, which set
    /// `ended`): wait on the child and join the stdout/stderr workers so the
    /// captured stderr is COMPLETE before it is read. Unlike `stop()` this does
    /// not kill the child — it has already exited. Without this, reading
    /// `stderr_text()` the instant `has_ended()` flips races the stderr thread
    /// still flushing, and the failure reason (device offline, unknown command…)
    /// can be lost.
    pub fn reap(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = self.child.wait();
        self.join_workers();
    }

    /// Join the stdout/stderr workers after the child has exited. Handles are
    /// consumed so a later `Drop` cannot attempt to join them twice.
    fn join_workers(&mut self) {
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
        self.reader_thread = None;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read `reader` line by line, decoding each line lossily (invalid UTF-8 bytes
/// become U+FFFD) and sending it — sans trailing CR/LF — through `tx` tagged
/// with `epoch`. `before_read` runs at the top of each iteration (used to park
/// while paused). Returns when the stream ends, a read errors, or `tx` closes.
///
/// This deliberately avoids `BufRead::lines()`: that decoder yields `Err` on the
/// first line with a non-UTF-8 byte, and the previous `map_while(Result::ok)`
/// turned that single bad line into a premature end-of-stream — silently losing
/// it and everything after. Lossy decoding keeps the line and the stream alive.
fn stream_lossy_lines<R: BufRead>(mut reader: R, mut emit: impl FnMut(String) -> bool) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF: stdout closed.
            Ok(_) => {}
            Err(_) => break, // genuine I/O error: stop reading.
        }
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        if !emit(line) {
            break; // receiver gone
        }
    }
}

/// Route one streamed line while honoring pause. While `paused`, buffer the line
/// (bounded by `cap`, dropping the oldest past the cap) instead of emitting it —
/// this keeps draining adb's stdout so the kernel pipe never backs up (a full
/// pipe makes the device's logd rotate our logs away). On resume, the buffered
/// backlog is flushed oldest-first, then the current line, via `send`. Returns
/// false when `send` fails (the receiver is gone), so the caller stops.
fn route_line(
    paused: bool,
    line: String,
    buffer: &mut std::collections::VecDeque<String>,
    cap: usize,
    mut send: impl FnMut(String) -> bool,
) -> bool {
    if paused {
        if buffer.len() >= cap {
            buffer.pop_front();
        }
        buffer.push_back(line);
        return true;
    }
    while let Some(buffered) = buffer.pop_front() {
        if !send(buffered) {
            return false;
        }
    }
    send(line)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stop_reaps_stdout_and_stderr_workers() {
        let (tx, rx) = crossbeam_channel::bounded(4);
        let mut session = Session::start(
            Some("/bin/sh"),
            None,
            "-c 'printf output; printf error >&2'",
            tx,
            1,
        )
        .expect("shell-backed session should start");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !session.has_ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.has_ended(),
            "stdout worker should observe process exit"
        );

        session.stop();

        assert_eq!(rx.try_recv(), Ok((1, "output".into())));
        assert_eq!(session.stderr_text(), "error");
        assert!(session.reader_handle.is_none());
        assert!(session.stderr_handle.is_none());
        assert!(session.reader_thread.is_none());
    }

    #[test]
    fn reap_drains_stderr_of_a_self_ended_session() {
        // A child that writes to stderr and exits. After it ends on its own,
        // reap() must join the stderr worker so the reason is fully captured —
        // this is the path the UI takes when has_ended() flips.
        let (tx, _rx) = crossbeam_channel::bounded(4);
        let mut session = Session::start(
            Some("/bin/sh"),
            None,
            "-c 'printf \"device offline\" >&2'",
            tx,
            1,
        )
        .expect("shell-backed session should start");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !session.has_ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.has_ended(), "process should end on its own");

        session.reap();
        assert_eq!(
            session.stderr_text(),
            "device offline",
            "reap must join the stderr worker so the reason is complete"
        );
        assert!(session.stderr_handle.is_none(), "stderr worker joined");
        assert!(session.reader_handle.is_none(), "stdout worker joined");
    }

    #[test]
    fn stream_lossy_lines_keeps_all_lines_across_invalid_utf8() {
        // A stray non-UTF-8 byte (0xFF) sits in the middle line. The old
        // lines()+map_while(ok) path would drop that line AND every line after
        // it. Lossy decoding must keep all three lines and replace the bad byte.
        let mut data = Vec::new();
        data.extend_from_slice(b"first\n");
        data.extend_from_slice(b"bad\xFFbyte\n");
        data.extend_from_slice(b"third\n");

        let (tx, rx) = crossbeam_channel::bounded(16);
        stream_lossy_lines(std::io::Cursor::new(data), |line| {
            tx.send((7, line)).is_ok()
        });
        drop(tx);

        let lines: Vec<(u64, String)> = rx.try_iter().collect();
        assert_eq!(lines.len(), 3, "no line should be dropped");
        assert_eq!(lines[0], (7, "first".to_string()));
        assert_eq!(lines[1].0, 7);
        assert!(
            lines[1].1.starts_with("bad") && lines[1].1.ends_with("byte"),
            "bad byte line kept with replacement char: {:?}",
            lines[1].1
        );
        assert!(lines[1].1.contains('\u{FFFD}'), "0xFF → U+FFFD");
        assert_eq!(lines[2], (7, "third".to_string()));
    }

    #[test]
    fn stream_lossy_lines_trims_crlf_and_handles_no_final_newline() {
        let data = b"has\r\nno-newline-tail".to_vec();
        let (tx, rx) = crossbeam_channel::bounded(16);
        stream_lossy_lines(std::io::Cursor::new(data), |line| {
            tx.send((1, line)).is_ok()
        });
        drop(tx);
        let lines: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(
            lines,
            vec!["has".to_string(), "no-newline-tail".to_string()]
        );
    }

    #[test]
    fn stream_lossy_lines_stops_when_receiver_dropped() {
        // If the ingest side goes away, the reader must stop rather than spin.
        let data = b"a\nb\nc\n".to_vec();
        let (tx, rx) = crossbeam_channel::bounded(16);
        drop(rx);
        stream_lossy_lines(std::io::Cursor::new(data), |line| {
            tx.send((1, line)).is_ok()
        });
        // Returning at all (no hang/panic) is the assertion.
    }

    #[test]
    fn route_line_buffers_while_paused_then_flushes_on_resume() {
        use std::collections::VecDeque;
        let mut buffer: VecDeque<String> = VecDeque::new();
        let mut out: Vec<String> = Vec::new();

        // Paused: lines are buffered, nothing emitted.
        route_line(true, "a".into(), &mut buffer, 3, |l| {
            out.push(l);
            true
        });
        route_line(true, "b".into(), &mut buffer, 3, |l| {
            out.push(l);
            true
        });
        assert!(out.is_empty(), "paused lines must not be emitted yet");
        assert_eq!(buffer.len(), 2);

        // Resume: the next line flushes the backlog (oldest first) then itself.
        route_line(false, "c".into(), &mut buffer, 3, |l| {
            out.push(l);
            true
        });
        assert_eq!(out, vec!["a", "b", "c"]);
        assert!(buffer.is_empty());

        // Overflow past the cap drops the oldest buffered line.
        for l in ["x", "y", "z", "w"] {
            route_line(true, l.into(), &mut buffer, 3, |_| true);
        }
        assert_eq!(
            buffer.iter().cloned().collect::<Vec<_>>(),
            vec!["y", "z", "w"],
            "cap=3 drops the oldest ('x')"
        );
    }
}
