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
        if pb.exists() { return pb; }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let candidate = PathBuf::from(local).join("Android/Sdk/platform-tools/adb.exe");
            if candidate.exists() { return candidate; }
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
        if line.is_empty() { continue; }
        // Skip informational lines that adb sometimes prints alongside real entries.
        if line.starts_with("List of devices") { continue; }
        if line.starts_with('*') { continue; }
        // Real entries look like "SERIAL\tdevice" or "SERIAL\toffline".
        if let Some((serial, state)) = line.split_once(|c: char| c.is_whitespace()) {
            let state = state.trim();
            if serial.is_empty() { continue; }
            // Keep all reachable entries; offline devices can still be shown so
            // the user knows about them, but drop unauthorized/permission ones.
            if state == "unauthorized" || state == "no permissions" { continue; }
            devices.push(serial.to_string());
        }
    }
    Ok(devices)
}

pub struct Session {
    child: Child,
    paused: Arc<std::sync::atomic::AtomicBool>,
    reader_thread: Option<thread::Thread>,
    #[allow(dead_code)]
    reader_handle: Option<thread::JoinHandle<()>>,
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
        let args = shlex::split(cmd).unwrap_or_else(|| cmd.split_whitespace().map(str::to_string).collect());
        for tok in args {
            command.arg(tok);
        }
        // Capture stderr so device/command errors are visible instead of being
        // inherited into the parent's console (invisible in a GUI build).
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()
            .map_err(|e| anyhow!("failed to spawn adb: {} (is adb on PATH?)", e))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused_thr = paused.clone();
        let ended = Arc::new(AtomicBool::new(false));
        let ended_thr = ended.clone();
        let handle = thread::Builder::new().name("adb-reader".into()).spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(|r| r.ok()) {
                // When paused, park the reader thread: it blocks here, neither
                // burning CPU nor consuming/discarding stdout data. The data
                // merely buffers in the kernel pipe until resumed.
                while paused_thr.load(std::sync::atomic::Ordering::Relaxed) {
                    thread::park();
                }
                if tx.send((epoch, line)).is_err() { break; }
            }
            // stdout closed → the adb process ended (or was killed). Signal the
            // UI so it doesn't keep showing a live session that emits nothing.
            ended_thr.store(true, Ordering::Release);
        })?;
        let reader_thread = handle.thread().clone();

        // Drain stderr on its own thread into a shared buffer.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        if let Some(mut es) = child.stderr.take() {
            let buf = stderr_buf.clone();
            let _ = thread::Builder::new().name("adb-stderr".into()).spawn(move || {
                let mut s = String::new();
                if es.read_to_string(&mut s).is_ok() && !s.is_empty() {
                    if let Ok(mut guard) = buf.lock() {
                        guard.push_str(&s);
                    }
                }
            });
        }

        Ok(Self {
            child,
            paused,
            reader_thread: Some(reader_thread),
            reader_handle: Some(handle),
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
        self.stderr.lock().map(|g| g.trim().to_string()).unwrap_or_default()
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
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Wake the reader so it can see the broken pipe / empty read and exit.
        if let Some(t) = &self.reader_thread {
            t.unpark();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}
