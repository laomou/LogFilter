use anyhow::{anyhow, Result};
use crossbeam_channel::Sender;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    paused: Arc<AtomicBool>,
    #[allow(dead_code)]
    reader_handle: Option<thread::JoinHandle<()>>,
}

impl Session {
    /// Spawn `adb [-s serial] <cmd_args...>` and stream stdout lines into `tx`.
    /// Blank lines are preserved; the reader thread exits when stdout closes.
    pub fn start(
        adb_override: Option<&str>,
        device: Option<&str>,
        cmd: &str,
        tx: Sender<String>,
    ) -> Result<Self> {
        let mut command = adb_command(adb_override);
        if let Some(d) = device {
            command.arg("-s").arg(d);
        }
        for tok in cmd.split_whitespace() {
            command.arg(tok);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()
            .map_err(|e| anyhow!("failed to spawn adb: {} (is adb on PATH?)", e))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let paused = Arc::new(AtomicBool::new(false));
        let paused_thr = paused.clone();
        let handle = thread::Builder::new().name("adb-reader".into()).spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(|r| r.ok()) {
                if paused_thr.load(Ordering::Relaxed) { continue; }
                if tx.send(line).is_err() { break; }
            }
        })?;
        Ok(Self {
            child,
            paused,
            reader_handle: Some(handle),
        })
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}
