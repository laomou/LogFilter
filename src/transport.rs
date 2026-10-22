use crate::lock::MutexExt;
use crate::model::LogFormat;
use crate::recording;
use anyhow::{anyhow, Result};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::io::Read;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Device-connector backend. adb (Android) and hdc (HarmonyOS) differ only in
/// the binary name, the command that lists devices, and the flag that selects
/// one — everything else (streaming, pause handling, stderr) is shared, so they
/// live in one module behind this enum rather than separate adb.rs/hdc.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Transport {
    #[default]
    Adb,
    Hdc,
}

impl Transport {
    /// Default binary name, resolved against PATH unless a path override is set.
    pub fn binary(self) -> &'static str {
        match self {
            Transport::Adb => "adb",
            Transport::Hdc => "hdc",
        }
    }
    /// Args that list connected devices (`adb devices` / `hdc list targets`).
    pub fn list_args(self) -> &'static [&'static str] {
        match self {
            Transport::Adb => &["devices"],
            Transport::Hdc => &["list", "targets"],
        }
    }
    /// Flag selecting a specific device by serial/connect-key.
    pub fn device_flag(self) -> &'static str {
        match self {
            Transport::Adb => "-s",
            Transport::Hdc => "-t",
        }
    }
    /// Human-readable label for the transport picker.
    pub fn label(self) -> &'static str {
        match self {
            Transport::Adb => "Android (adb)",
            Transport::Hdc => "HarmonyOS (hdc)",
        }
    }
}

/// A command shipped in the (non-editable) command dropdown, tagged with the
/// transport it belongs to and the log format it produces. This table is the
/// single source of truth: the default per-backend command lists
/// ([`default_commands`]) and format detection ([`builtin_command`]) both derive
/// from it, rather than sniffing the command string for `"hilog"`/`"logcat"`.
pub struct BuiltinCommand {
    pub cmd: &'static str,
    /// The backend this command runs on (each shipped command targets exactly one).
    pub transport: Transport,
    pub format: LogFormat,
}

/// The built-in command set. Order here is the per-backend dropdown order; the
/// kernel-log command is kept last within the adb list.
pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        cmd: "logcat -v threadtime",
        transport: Transport::Adb,
        format: LogFormat::ThreadTime,
    },
    BuiltinCommand {
        cmd: "logcat -v time",
        transport: Transport::Adb,
        format: LogFormat::Time,
    },
    BuiltinCommand {
        cmd: "logcat -b radio -v time",
        transport: Transport::Adb,
        format: LogFormat::Time,
    },
    BuiltinCommand {
        cmd: "logcat -b events -v time",
        transport: Transport::Adb,
        format: LogFormat::Time,
    },
    // Plain `hilog` (optionally with an explicit format) does a *blocking
    // streaming* read. Older presets used `-r`, which per the official hilog
    // source **clears the device log buffer and exits** — destructive for a
    // capture tool — and `threadtime`, which is not a valid hilog `-v` value.
    BuiltinCommand {
        cmd: "hilog",
        transport: Transport::Hdc,
        format: LogFormat::HiLog,
    },
    BuiltinCommand {
        cmd: "hilog -v time",
        transport: Transport::Hdc,
        format: LogFormat::HiLog,
    },
    BuiltinCommand {
        cmd: "hilog -D 0x2F00 -v time",
        transport: Transport::Hdc,
        format: LogFormat::HiLog,
    },
    BuiltinCommand {
        cmd: "hilog -D 0x2D00 -v time",
        transport: Transport::Hdc,
        format: LogFormat::HiLog,
    },
    BuiltinCommand {
        cmd: "shell cat /proc/kmsg",
        // Android-only: HarmonyOS has no /proc/kmsg (hdc reports "No such file").
        transport: Transport::Adb,
        format: LogFormat::Kernel,
    },
];

/// Look up a command in [`BUILTIN_COMMANDS`] by exact match.
pub fn builtin_command(cmd: &str) -> Option<&'static BuiltinCommand> {
    BUILTIN_COMMANDS.iter().find(|b| b.cmd == cmd)
}

/// Built-in commands for a transport, in dropdown order. Used for the default
/// per-backend command lists and as the fallback when switching backends.
pub fn default_commands(transport: Transport) -> Vec<String> {
    BUILTIN_COMMANDS
        .iter()
        .filter(|b| b.transport == transport)
        .map(|b| b.cmd.to_string())
        .collect()
}

/// Presets shipped by older versions that turned out to be broken: `-r` clears
/// the device hilog buffer (destructive) and `threadtime` is not a valid hilog
/// `-v` value. Configs saved before the fix can still contain them — and a user
/// can retype one — so they resolve to the closest safe streaming preset.
pub fn legacy_hdc_preset(cmd: &str) -> Option<&'static str> {
    match cmd {
        "hilog -v threadtime -r" => Some("hilog"),
        "hilog -v time -r" => Some("hilog -v time"),
        _ => None,
    }
}

/// Resolve only our built-in hdc presets on the device. hilogcat does not
/// share hilog's option grammar, so its fallback uses default streaming and
/// explicitly reports when a domain preset cannot be applied.
fn device_command_args(transport: Transport, cmd: &str) -> Result<Vec<String>> {
    let args = shlex::split(cmd)
        .ok_or_else(|| anyhow!("unbalanced quotes or trailing backslash in command: {cmd}"))?;
    if transport != Transport::Hdc {
        return Ok(args);
    }
    // A legacy preset is rewritten to its safe replacement so the wrapper (and
    // the hilogcat fallback) still apply to configs saved before the fix.
    let effective_cmd = match legacy_hdc_preset(cmd) {
        Some(replacement) => replacement,
        None => match builtin_command(cmd) {
            Some(_) => cmd,
            None => return Ok(args),
        },
    };
    let effective_args = shlex::split(effective_cmd)
        .ok_or_else(|| anyhow!("malformed built-in command: {effective_cmd}"))?;
    let primary = effective_args
        .iter()
        .map(|arg| shlex::try_quote(arg).map(|q| q.into_owned()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(" ");
    let notice = if effective_args.iter().any(|a| a == "-D") {
        "LogFilter: hilog unavailable; using hilogcat defaults (domain preset not supported)"
    } else {
        "LogFilter: hilog unavailable; using hilogcat default output"
    };
    let script = format!(
        "if command -v hilog >/dev/null 2>&1; then exec {primary}; \
         elif command -v hilogcat >/dev/null 2>&1; then echo '{}' >&2; exec hilogcat; \
         else echo 'LogFilter: neither hilog nor hilogcat is available' >&2; exit 127; fi",
        notice,
    );
    Ok(vec!["shell".into(), script])
}

/// Windows CREATE_NO_WINDOW: suppresses the black cmd.exe window that would
/// otherwise flash for every child process when running as a GUI (windows
/// subsystem) executable.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Build a Command for the transport's binary, suppressing console popups on Windows.
fn transport_command(transport: Transport, override_path: Option<&str>) -> Result<Command> {
    #[allow(unused_mut)]
    let mut cmd = Command::new(binary_path(transport, override_path)?);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

/// Locate the transport's executable (adb / hdc). If the user set an explicit
/// path in config, use that. Otherwise return the bare name and let
/// `std::process::Command` resolve it against PATH (Rust searches PATHEXT on
/// Windows). Windows-only fallback for adb: the Android Studio install location.
///
/// An override that does not name a runnable file is an error rather than a
/// fallback. Silently dropping back to PATH would run a *different* adb, with
/// its own server and its own set of visible devices, which is the one outcome
/// the override exists to prevent; a typo would look like "no devices found".
pub fn binary_path(transport: Transport, override_path: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        let pb = PathBuf::from(p);
        // `is_file` rather than `exists`: a directory satisfies `exists`, then
        // fails to spawn with EACCES, which the caller reports as "is it on
        // PATH?" — pointing the user away from the actual mistake.
        if !pb.is_file() {
            return Err(anyhow!(
                "configured {} path is not a runnable file: {}",
                transport.binary(),
                pb.display()
            ));
        }
        return Ok(pb);
    }
    #[cfg(target_os = "windows")]
    {
        if transport == Transport::Adb {
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                let candidate = PathBuf::from(local).join("Android/Sdk/platform-tools/adb.exe");
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }
    Ok(PathBuf::from(transport.binary()))
}

pub fn list_devices(transport: Transport, override_path: Option<&str>) -> Result<Vec<String>> {
    let bin = transport.binary();
    let out = transport_command(transport, override_path)?
        .args(transport.list_args())
        .output()
        .map_err(|e| anyhow!("failed to spawn `{bin}`: {e} (is {bin} on PATH?)"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "`{bin} {}` exited with {:?}: {}",
            transport.list_args().join(" "),
            out.status.code(),
            stderr.trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // hdc reports daemon/protocol errors (no matching target, device dropped)
    // as `[Fail] …` lines on **stdout** with exit code 0 — so the success
    // check above is dead for hdc. Surface the failure instead of letting the
    // error text be parsed as a device list.
    if transport == Transport::Hdc {
        if let Some(line) = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("[Fail]"))
        {
            return Err(anyhow!(
                "`{bin} {}` failed: {line}",
                transport.list_args().join(" ")
            ));
        }
    }
    Ok(match transport {
        Transport::Adb => parse_device_lines(&text),
        Transport::Hdc => parse_hdc_targets(&text),
    })
}

/// Parse `hdc list targets` output: one connect key per line, or `[Empty]` when
/// no device is attached. `[Fail]` protocol-error lines are never devices.
fn parse_hdc_targets(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "[Empty]" && !l.starts_with("[Fail]"))
        .map(String::from)
        .collect()
}

/// Parse `adb devices` output into display entries. A ready device is just its
/// serial; any other state (unauthorized, offline, no permissions, recovery…) is
/// labeled `"SERIAL (state)"` so the user *sees* the device and why it may not
/// work — rather than it vanishing (a freshly plugged-in phone is "unauthorized"
/// until the user taps "allow", and hiding it looks like "no devices / broken").
/// Use [`device_serial`] to recover the bare serial for `adb -s`.
fn parse_device_lines(text: &str) -> Vec<String> {
    let mut devices = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        // Real entries look like "SERIAL\tdevice" or "SERIAL\tunauthorized".
        if let Some((serial, state)) = line.split_once(|c: char| c.is_whitespace()) {
            let state = state.trim();
            if serial.is_empty() {
                continue;
            }
            if state == "device" {
                devices.push(serial.to_string());
            } else {
                devices.push(format!("{serial} ({state})"));
            }
        }
    }
    devices
}

/// Recover the bare serial from a (possibly labeled) device entry, e.g.
/// `"emulator-5554 (unauthorized)"` → `"emulator-5554"`. adb serials never
/// contain a space, so splitting on `" ("` is safe.
pub fn device_serial(entry: &str) -> &str {
    entry.split(" (").next().unwrap_or(entry)
}

/// Kills and reaps a freshly spawned child unless the session takes ownership.
///
/// `Child::drop` neither kills nor waits, so a failure between `spawn()` and the
/// point where the `Session` owns the child would leave a live adb/hdc process
/// behind. Dropping this guard on the error path terminates it instead.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn get(&mut self) -> &mut Child {
        self.0
            .as_mut()
            .expect("child is present until the session takes it")
    }

    fn take(&mut self) -> Child {
        self.0
            .take()
            .expect("child is present until the session takes it")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub struct Session {
    child: Option<Child>,
    /// Private to this session: clearing advances it without letting an old
    /// reader adopt the epoch of a replacement file or device session.
    epoch: Arc<AtomicU64>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    reader_thread: Option<thread::Thread>,
    reader_handle: Option<thread::JoinHandle<()>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
    /// Stop forwarding lines while asynchronous cleanup waits for pipe closure.
    stop_requested: Arc<AtomicBool>,
    /// Captured stderr from the adb child. Populated by a reader thread; read by
    /// the UI when the session ends to surface the reason (device offline,
    /// unknown command, etc.) instead of a silent empty stream.
    stderr: Arc<Mutex<String>>,
    /// Why the reader stopped, when it was not a normal end of stream. `None`
    /// means EOF: the device or the command simply finished. A failed stream and
    /// a finished one look identical from outside (stdout just stops being
    /// readable), so without this the UI can only say "ended".
    read_error: Arc<Mutex<Option<String>>>,
    /// When the child was first observed exited, used to bound how long
    /// completion waits on the stderr worker. `None` until then, and reset if a
    /// later `try_wait` shows the child still running.
    exited_at: Option<std::time::Instant>,
}

/// How long completion waits for the stderr worker to reach EOF after the child
/// has exited and stdout has ended. Long enough for the ordinary case where the
/// pipe closes a moment after the process, short enough not to strand a session
/// whose descendant never closes it.
const STDERR_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Poll pipe readiness instead of blocking indefinitely on a silent descendant.
/// This reader is the pipe's sole consumer, so reading only ready bytes is safe.
struct CancellableStdout {
    pipe: std::process::ChildStdout,
    stop: Arc<AtomicBool>,
}

impl Read for CancellableStdout {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let mut fd = libc::pollfd {
                    fd: self.pipe.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: fd owns valid pollfd storage and the pipe remains open.
                let ready = unsafe { libc::poll(&mut fd, 1, 50) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready > 0 {
                    if self.stop.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    return self.pipe.read(buf);
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
                use windows_sys::Win32::System::Pipes::PeekNamedPipe;
                let mut available = 0;
                // SAFETY: the pipe owns a valid handle and available is writable.
                let ok = unsafe {
                    PeekNamedPipe(
                        self.pipe.as_raw_handle(),
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                        return Ok(0);
                    }
                    return Err(error);
                }
                if available > 0 {
                    if self.stop.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    let count = buf.len().min(available as usize);
                    return self.pipe.read(&mut buf[..count]);
                }
                thread::sleep(std::time::Duration::from_millis(50));
            }
            #[cfg(not(any(unix, windows)))]
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "cancellable device pipes are unsupported on this platform",
            ));
        }
    }
}

/// hdc reports protocol errors — no matching target, device dropped — by
/// printing a `[Fail] …` line to **stdout** and still exiting 0. Without this
/// guard such a session records the error text as one row of log data and the
/// UI reports an ordinary "session ended".
///
/// The guard inspects the first line of the stream. A `[Fail]` line is
/// published into the session's `read_error` slot (surfaced by the existing
/// "capture failed: …" status) and nothing is recorded; any other first line is
/// passed through untouched, followed by the rest of the stream.
struct HdcFailureGuard<R: Read> {
    inner: R,
    /// First-line bytes pulled in but not yet passed through.
    pending: Vec<u8>,
    checked: bool,
    failed: bool,
    read_error: Arc<Mutex<Option<String>>>,
}

impl<R: Read> HdcFailureGuard<R> {
    fn new(inner: R, read_error: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            checked: false,
            failed: false,
            read_error,
        }
    }
}

impl<R: Read> Read for HdcFailureGuard<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.failed {
            return Ok(0);
        }
        if !self.checked {
            self.checked = true;
            // Pull until the first newline (protocol-error lines are short; the
            // cap keeps a pathological stream from being buffered forever).
            let mut tmp = [0u8; 4096];
            loop {
                if self.pending.contains(&b'\n') || self.pending.len() >= 8192 {
                    break;
                }
                let n = self.inner.read(&mut tmp)?;
                if n == 0 {
                    break;
                }
                self.pending.extend_from_slice(&tmp[..n]);
            }
            let first_end = self
                .pending
                .iter()
                .position(|&b| b == b'\n')
                .map_or(self.pending.len(), |i| i);
            let first_line = String::from_utf8_lossy(&self.pending[..first_end]);
            if first_line.trim_start().starts_with("[Fail]") {
                self.failed = true;
                *self.read_error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(first_line.trim().to_string());
                return Ok(0);
            }
        }
        if !self.pending.is_empty() {
            let n = self.pending.len().min(buf.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

enum Output {
    Recording(recording::Writer),
    #[cfg(test)]
    Lines(Sender<(u64, String)>),
}

impl Session {
    /// Spawn `<binary> [device-flag serial] <cmd_args...>` and stream stdout
    /// lines into `tx`, each tagged with `epoch` so the ingest side can drop
    /// lines from a superseded source. Blank lines are preserved; the reader
    /// thread exits when stdout closes.
    #[cfg(test)]
    pub fn start(
        transport: Transport,
        override_path: Option<&str>,
        device: Option<&str>,
        cmd: &str,
        tx: Sender<(u64, String)>,
        epoch: u64,
    ) -> Result<Self> {
        Self::start_output(
            transport,
            override_path,
            device,
            cmd,
            Output::Lines(tx),
            epoch,
        )
    }

    pub fn start_recording(
        transport: Transport,
        override_path: Option<&str>,
        device: Option<&str>,
        cmd: &str,
        writer: recording::Writer,
    ) -> Result<Self> {
        Self::start_output(
            transport,
            override_path,
            device,
            cmd,
            Output::Recording(writer),
            0,
        )
    }

    fn start_output(
        transport: Transport,
        override_path: Option<&str>,
        device: Option<&str>,
        cmd: &str,
        output: Output,
        epoch: u64,
    ) -> Result<Self> {
        let mut command = transport_command(transport, override_path)?;
        if let Some(d) = device {
            command.arg(transport.device_flag()).arg(d);
        }
        // Split the command string respecting shell quoting (e.g. `logcat -s "My Tag"`).
        //
        // `shlex::split` returns None precisely when the string is not valid
        // shell syntax — an unbalanced quote or a trailing backslash. Falling
        // back to a whitespace split would pass the literal quote through as
        // argv (`-s "My Tag` becomes `-s`, `"My`, `Tag`), so the device filters
        // on the tag `"My` plus a stray operand and the user sees nothing but
        // "session ended". There is no injection risk either way — no shell is
        // involved — but a refusal is the only honest answer.
        let args = device_command_args(transport, cmd)?;
        for tok in args {
            command.arg(tok);
        }
        // Capture stderr so device/command errors are visible instead of being
        // inherited into the parent's console (invisible in a GUI build).
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // `Child::drop` neither kills nor waits, so without this guard every
        // fallible step between `spawn()` and the session taking ownership
        // (pipe setup, worker-thread spawn) would leak a running adb/hdc — and a
        // stderr-thread failure would additionally leave the already-started
        // reader draining a recording that nothing can ever stop.
        let mut child = ChildGuard::new(command.spawn().map_err(|e| {
            anyhow!(
                "failed to spawn {}: {e} (is it on PATH?)",
                transport.binary()
            )
        })?);
        let stdout = child
            .get()
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout"))?;
        let paused = match &output {
            Output::Recording(writer) => writer.state.paused.clone(),
            #[cfg(test)]
            Output::Lines(_) => Arc::new(AtomicBool::new(false)),
        };
        #[cfg(test)]
        let paused_thr = paused.clone();
        let read_error = Arc::new(Mutex::new(None));
        let read_error_thr = read_error.clone();
        let stop_requested = match &output {
            Output::Recording(writer) => writer.state.stop.clone(),
            #[cfg(test)]
            Output::Lines(_) => Arc::new(AtomicBool::new(false)),
        };
        let stdout = CancellableStdout {
            pipe: stdout,
            stop: stop_requested.clone(),
        };
        #[cfg(test)]
        let reader_stop_requested = stop_requested.clone();
        let epoch = Arc::new(AtomicU64::new(epoch));
        #[cfg(test)]
        let reader_epoch = epoch.clone();
        // See `HdcFailureGuard`: for hdc, session failures arrive on stdout and
        // are published into `read_error` so the UI reports "capture failed"
        // instead of an ordinary "session ended".
        let stdout: Box<dyn Read + Send> = if transport == Transport::Hdc {
            Box::new(HdcFailureGuard::new(stdout, read_error_thr.clone()))
        } else {
            Box::new(stdout)
        };
        let handle = thread::Builder::new()
            .name("device-reader".into())
            .spawn(move || {
                match output {
                    Output::Recording(writer) => {
                        // Disk I/O is the only backpressure on the device pipe.
                        // Filters and rendering never gate this path. Pause keeps
                        // reading stdout but Writer deliberately skips disk writes.
                        let state = writer.state.clone();
                        writer.record(stdout);
                        let error = state.error.lock_recover().clone();
                        if let Some(error) = error {
                            *read_error_thr.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                        }
                    }
                    #[cfg(test)]
                    Output::Lines(tx) => {
                        let reader = BufReader::new(stdout);
                        let outcome = stream_lossy_lines(reader, |line| {
                            if reader_stop_requested.load(Ordering::Acquire) {
                                return false;
                            }
                            // "Pause Capture" stops *recording*, not *reading*: keep
                            // draining the kernel pipe so adb doesn't back up and the
                            // device's logd doesn't rotate our logs away.
                            if paused_thr.load(Ordering::Relaxed) {
                                return true;
                            }
                            // Test-only legacy line sink for the epoch/cancellation tests.
                            // Production device capture always uses Output::Recording.
                            tx.send((reader_epoch.load(Ordering::Acquire), line))
                                .is_ok()
                        });
                        if let Err(e) = outcome {
                            // Record why, so the UI can say "capture failed" rather than
                            // reporting an I/O failure as an ordinary end of session.
                            if let Ok(mut slot) = read_error_thr.lock() {
                                *slot = Some(e.to_string());
                            }
                        }
                    }
                }
            })?;
        let reader_thread = handle.thread().clone();

        // Drain stderr on its own thread into a shared buffer.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_handle = if let Some(mut es) = child.get().stderr.take() {
            let buf = stderr_buf.clone();
            Some(
                thread::Builder::new()
                    .name("device-stderr".into())
                    .spawn(move || {
                        // Diagnostics are bounded too; keep draining after the
                        // retained prefix fills so stderr never blocks capture.
                        let mut retained = Vec::new();
                        let mut block = [0u8; 8192];
                        loop {
                            match es.read(&mut block) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let keep =
                                        n.min((256 * 1024usize).saturating_sub(retained.len()));
                                    if keep == 0 {
                                        continue;
                                    }
                                    retained.extend_from_slice(&block[..keep]);
                                    // Publish as it arrives, not at EOF: a
                                    // descendant can hold this pipe open for the
                                    // whole life of the session, and the reason a
                                    // capture is failing (device offline, unknown
                                    // command) is needed while it is still running.
                                    *buf.lock_recover() =
                                        String::from_utf8_lossy(&retained).into_owned();
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(_) => break,
                            }
                        }
                        *buf.lock_recover() = String::from_utf8_lossy(&retained).into_owned();
                    })?,
            )
        } else {
            None
        };

        Ok(Self {
            child: Some(child.take()),
            epoch,
            paused,
            reader_thread: Some(reader_thread),
            reader_handle: Some(handle),
            stderr_handle,
            stop_requested,
            stderr: stderr_buf,
            read_error,
            exited_at: None,
        })
    }

    /// Tag future lines with the epoch installed by the UI's clear operation.
    pub fn set_epoch(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::Release);
    }

    /// The stdout reader has finished. This does not imply the child exited:
    /// a command can close stdout and keep running or only write to stderr.
    pub fn stdout_ended(&self) -> bool {
        self.reader_handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// The I/O error that stopped the reader, if it stopped for that reason.
    ///
    /// `None` means the stream ended normally (EOF); see [`Self::stdout_ended`].
    /// A poisoned lock still yields its value: the writer sets this *before* it
    /// can panic elsewhere, and reporting "ended normally" for a capture that
    /// failed is the one answer that is certainly wrong.
    pub fn read_error_text(&self) -> Option<String> {
        self.read_error.lock_recover().clone()
    }

    /// Snapshot of any stderr captured from the adb child so far.
    pub fn stderr_text(&self) -> String {
        self.stderr.lock_recover().trim().to_string()
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
        let Some(mut child) = self.child.take() else {
            return;
        };
        self.stop_requested.store(true, Ordering::Release);
        let _ = child.kill();
        let reader = self.reader_handle.take();
        let stderr = self.stderr_handle.take();
        self.reader_thread = None;
        // Neither process exit nor pipe closure is guaranteed to be immediate.
        // In particular, descendants may retain stdout/stderr after the direct
        // child dies. Stop, Restart, file-open and Drop must never join on the UI.
        // The child lives in a slot shared with the error path: the worker
        // takes it out when it runs, and if spawning the worker itself fails
        // the slot is what lets stop() recover the child instead of dropping
        // it (a dropped `Child` is never reaped by std and stays a zombie).
        let child_slot = Arc::new(Mutex::new(Some(child)));
        if let Err(e) = thread::Builder::new().name("device-reap".into()).spawn({
            let slot = child_slot.clone();
            move || {
                if let Some(mut child) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    let _ = child.wait();
                }
                if let Some(handle) = reader {
                    let _ = handle.join();
                }
                if let Some(handle) = stderr {
                    let _ = handle.join();
                }
            }
        }) {
            // Degraded cleanup. `wait()` is off the table — stop must never
            // block the UI — so:
            //  1. reap inline if the child has already exited (non-blocking);
            //  2. otherwise hand the child back to the session so the next
            //     `stop()`/`try_reap()` — including the session's `Drop` when
            //     the UI releases it — retries the cleanup.
            // The reader/stderr handles are abandoned: those threads exit on
            // pipe EOF on their own, and the UI has stopped polling them.
            // The slot is shared with the worker above, but a spawn failure
            // means the worker never ran, so the child is still here — unless
            // this ever becomes impossible, in which case there is simply
            // nothing left to recover.
            let child = child_slot.lock().unwrap_or_else(|p| p.into_inner()).take();
            match child {
                Some(mut child) => {
                    let reaped = child.try_wait().is_ok_and(|status| status.is_some());
                    if !reaped {
                        self.child = Some(child);
                    }
                    tracing::warn!(
                        "failed to spawn device cleanup worker: {e}; {}",
                        if reaped {
                            "child was reaped inline"
                        } else {
                            "child kept for the next reap poll"
                        }
                    );
                }
                None => tracing::warn!(
                    "failed to spawn device cleanup worker: {e}; no child handle left to recover"
                ),
            }
        }
    }

    /// Non-blocking natural completion check, safe to call from the UI. Keep
    /// the session stoppable until both the process and its pipe workers end.
    /// Completed workers are joined before the UI reads their final diagnostics.
    ///
    /// The stderr worker only gets a short grace period, not a veto: a surviving
    /// descendant can hold that pipe open indefinitely (see
    /// `inherited_stderr_does_not_block_completion`), and waiting for its EOF
    /// would keep every exited session "running" forever — no end-of-capture in
    /// the UI, and the worker and its buffer never released. Since stderr is now
    /// published as it arrives, giving up on the EOF costs no diagnostics.
    ///
    /// The grace period is measured against these calls, not in the background:
    /// while the UI is not rendering (window hidden or the event loop starved)
    /// nothing polls, so completion is deferred to the next repaint rather than
    /// lost — the UI is the only consumer of it, and `exited_at` is real time,
    /// so a long deferral costs nothing on the first poll after it.
    pub fn try_reap(&mut self) -> std::io::Result<bool> {
        if self.stop_requested.load(Ordering::Acquire) {
            self.stop();
            return Ok(true);
        }
        let Some(child) = &mut self.child else {
            return Ok(true);
        };
        if child.try_wait()?.is_none() {
            self.exited_at = None;
            return Ok(false);
        }
        if !self.stdout_ended() {
            return Ok(false);
        }
        let stderr_pending = self
            .stderr_handle
            .as_ref()
            .is_some_and(|h| !h.is_finished());
        if stderr_pending {
            let waited = self
                .exited_at
                .get_or_insert_with(std::time::Instant::now)
                .elapsed();
            if waited < STDERR_GRACE {
                return Ok(false);
            }
        }
        self.release_workers();
        self.child = None;
        self.exited_at = None;
        Ok(true)
    }

    /// Join the workers that have finished and hand any straggler to a detached
    /// thread, so this never blocks the UI.
    fn release_workers(&mut self) {
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        self.reader_thread = None;
        let Some(stderr) = self.stderr_handle.take() else {
            return;
        };
        if stderr.is_finished() {
            let _ = stderr.join();
        } else if let Err(e) = thread::Builder::new()
            .name("device-stderr-reap".into())
            .spawn(move || {
                let _ = stderr.join();
            })
        {
            // Degraded cleanup: abandon the drain thread instead of blocking
            // the UI on `join()`. It exits on its own when the pipe's last
            // holder closes it (the child is already gone), and the captured
            // stderr text stays readable through `stderr_text()` because the
            // buffer is shared — only this thread handle leaks, and only for
            // as long as a descendant keeps the pipe open.
            tracing::warn!("failed to spawn stderr cleanup worker: {e}; abandoning drain thread");
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One streamed device line, tagged with the epoch of the source session that
/// produced it, so the ingest thread can drop lines from a superseded one.
pub type LogLine = (u64, String);

/// Local-file loader channel, throttled by io::send_throttled. Production device
/// capture does not use this channel: raw bytes go directly to the recording
/// file, and preview batches use recording::preview's bounded single-slot queue.
pub fn log_channel() -> (Sender<LogLine>, Receiver<LogLine>) {
    crossbeam_channel::unbounded()
}

/// Read `reader` line by line, decoding each line lossily (invalid UTF-8 bytes
/// become U+FFFD) and handing it — sans trailing CR/LF — to `emit`, which returns
/// false to stop.
///
/// Returns `Ok(())` when the stream ended normally (EOF) or `emit` asked to stop,
/// and the I/O error otherwise — so the caller can tell a finished stream apart
/// from a failed one instead of reporting both as "ended".
///
/// This deliberately avoids `BufRead::lines()`: that decoder yields `Err` on the
/// first line with a non-UTF-8 byte, and the previous `map_while(Result::ok)`
/// turned that single bad line into a premature end-of-stream — silently losing
/// it and everything after. Lossy decoding keeps the line and the stream alive.
#[cfg(test)]
fn stream_lossy_lines<R: BufRead>(
    mut reader: R,
    mut emit: impl FnMut(String) -> bool,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // No `Interrupted` handling is needed here: `read_until` already loops on
        // `fill_buf` internally, so a signal never surfaces as an error from it.
        // Anything that reaches this match is a real read failure.
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return Ok(()), // EOF: stdout closed.
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        if !emit(line) {
            return Ok(()); // receiver gone
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not finish"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn wait_for_reap(session: &mut Session) {
        wait_until(|| session.try_reap().unwrap());
    }

    fn gate_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lf_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn stop_finishes_recording_while_a_silent_descendant_holds_stdout() {
        for paused in [false, true] {
            let dir = gate_path("silent_stdout");
            let writer = recording::Writer::create(&dir).unwrap();
            let state = writer.state.clone();
            // sleep inherits stdout and survives the direct shell's death.
            // Its pid is reported on stderr so cleanup always releases it.
            let script = "sleep 30 & printf '%s' $! >&2; printf 'prefix\\n'; wait";
            let cmd = format!("-c {}", shlex::try_quote(script).unwrap());
            let mut session =
                Session::start_recording(Transport::Adb, Some("/bin/sh"), None, &cmd, writer)
                    .unwrap();
            wait_until(|| !session.stderr_text().is_empty());
            let pid: i32 = session.stderr_text().parse().unwrap();
            wait_until(|| state.written.load(Ordering::Acquire) >= 7);
            state.paused.store(paused, Ordering::Release);
            session.stop();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !state.finished.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            let finished = state.finished.load(Ordering::Acquire);
            // SAFETY: pid is the sleep child created by this test, not a process group.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            assert!(
                finished,
                "Stop must not depend on a descendant closing stdout"
            );
            assert!(state.error.lock_recover().is_none());
            assert_eq!(std::fs::read(&state.path).unwrap(), b"prefix\n");
            std::fs::remove_file(&state.path).unwrap();
            std::fs::remove_dir(&dir).unwrap();
        }
    }

    #[test]
    fn hdc_fallback_script_executes_hilogcat_and_reports_missing_tools() {
        use std::os::unix::fs::PermissionsExt;
        let dir = gate_path("hdc_tools");
        std::fs::create_dir(&dir).unwrap();
        let tool = dir.join("hilogcat");
        std::fs::write(
            &tool,
            "#!/bin/sh\nprintf '01-01 12:34:56.789 1 2 I 00400/Tag: next log\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = device_command_args(Transport::Hdc, "hilog -D 0x2F00 -v time").unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&args[1])
            .env("PATH", &dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("domain preset not supported"));
        let (_, format) =
            crate::parser::parse_line(String::from_utf8(output.stdout).unwrap().trim_end().into());
        assert_eq!(format, LogFormat::HiLog);
        std::fs::remove_file(&tool).unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&args[1])
            .env("PATH", &dir)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(127));
        assert!(String::from_utf8_lossy(&output.stderr).contains("neither hilog nor hilogcat"));
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn hdc_presets_resolve_device_tools_without_reusing_hilogcat_flags() {
        for cmd in default_commands(Transport::Hdc) {
            let args = device_command_args(Transport::Hdc, &cmd).unwrap();
            assert_eq!(args[0], "shell");
            assert!(args[1].contains(&format!("exec {cmd};")));
            assert!(args[1].contains("exec hilogcat;"));
            assert!(!args[1].contains("exec hilogcat -"));
            if cmd.contains("-D") {
                assert!(args[1].contains("domain preset not supported"));
            }
        }
        assert_eq!(
            device_command_args(Transport::Hdc, "shell echo custom").unwrap(),
            ["shell", "echo", "custom"]
        );
        assert_eq!(
            device_command_args(Transport::Adb, "logcat -v threadtime").unwrap(),
            ["logcat", "-v", "threadtime"]
        );
    }

    #[test]
    fn legacy_hdc_presets_resolve_to_safe_streaming() {
        // Configs saved before the fix (or retyped by hand) still carry the
        // old presets, whose `-r` flag CLEARS the device buffer. They must
        // resolve to the safe streaming equivalents, never run verbatim.
        let args = device_command_args(Transport::Hdc, "hilog -v time -r").unwrap();
        assert_eq!(args[0], "shell");
        assert!(args[1].contains("exec hilog -v time;"), "{}", args[1]);
        assert!(!args[1].contains("-r"), "{}", args[1]);

        let args = device_command_args(Transport::Hdc, "hilog -v threadtime -r").unwrap();
        assert!(args[1].contains("exec hilog;"), "{}", args[1]);
        assert!(!args[1].contains("threadtime"), "{}", args[1]);

        // A different command that merely contains the old string untouched.
        assert_eq!(
            device_command_args(Transport::Hdc, "hilog -v time -r extra").unwrap(),
            ["hilog", "-v", "time", "-r", "extra"]
        );
    }

    #[test]
    fn parse_hdc_targets_skips_failure_lines() {
        assert_eq!(parse_hdc_targets("[Empty]"), Vec::<String>::new());
        assert_eq!(
            parse_hdc_targets("FAKE123\n[Fail]Not match target founded\n"),
            vec!["FAKE123"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_devices_reports_hdc_stdout_failures() {
        use std::os::unix::fs::PermissionsExt;
        let dir = gate_path("hdc_list_fail");
        std::fs::create_dir(&dir).unwrap();
        let bin = dir.join("hdc");
        // hdc exits 0 even when the daemon reports a protocol failure; the
        // error text arrives on stdout.
        std::fs::write(
            &bin,
            "#!/bin/sh\necho '[Fail]Not match target founded, check connect-key please'\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = list_devices(Transport::Hdc, Some(bin.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("[Fail]"), "{err}");

        // A plain empty list is still a normal result.
        std::fs::write(&bin, "#!/bin/sh\necho '[Empty]'\n").unwrap();
        assert_eq!(
            list_devices(Transport::Hdc, Some(bin.to_str().unwrap())).unwrap(),
            Vec::<String>::new()
        );
        std::fs::remove_file(&bin).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hdc_fail_on_stdout_is_reported_not_recorded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = gate_path("hdc_capture_fail");
        std::fs::create_dir(&dir).unwrap();
        let bin = dir.join("hdc");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho '[Fail]Not match target founded, check connect-key please'\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let writer = recording::Writer::create(&dir).unwrap();
        let state = writer.state.clone();
        let mut session = Session::start_recording(
            Transport::Hdc,
            Some(bin.to_str().unwrap()),
            None,
            "hilog",
            writer,
        )
        .unwrap();
        // The failure text must surface as a diagnostic, not vanish.
        wait_until(|| {
            session
                .read_error_text()
                .is_some_and(|e| e.contains("[Fail]"))
        });
        wait_for_reap(&mut session);
        // And it must not be recorded as a row of log data.
        wait_until(|| state.finished.load(Ordering::Acquire));
        assert_eq!(std::fs::read(&state.path).unwrap(), Vec::<u8>::new());
        std::fs::remove_file(&state.path).unwrap();
        std::fs::remove_file(&bin).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hdc_guard_passes_real_log_lines_through() {
        use std::os::unix::fs::PermissionsExt;
        let dir = gate_path("hdc_capture_ok");
        std::fs::create_dir(&dir).unwrap();
        let bin = dir.join("hdc");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho '01-01 12:34:56.789 1 2 I 00400/Tag: first line'\necho '01-01 12:34:56.790 1 2 I 00400/Tag: second line'\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let writer = recording::Writer::create(&dir).unwrap();
        let state = writer.state.clone();
        let mut session = Session::start_recording(
            Transport::Hdc,
            Some(bin.to_str().unwrap()),
            None,
            "hilog",
            writer,
        )
        .unwrap();
        wait_for_reap(&mut session);
        wait_until(|| state.finished.load(Ordering::Acquire));
        assert_eq!(
            std::fs::read(&state.path).unwrap(),
            b"01-01 12:34:56.789 1 2 I 00400/Tag: first line\n01-01 12:34:56.790 1 2 I 00400/Tag: second line\n"
        );
        assert!(session.read_error_text().is_none());
        std::fs::remove_file(&state.path).unwrap();
        std::fs::remove_file(&bin).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn parse_device_lines_labels_non_ready_states() {
        let out = "List of devices attached\n\
                   emulator-5554\tdevice\n\
                   ABC123\tunauthorized\n\
                   DEF456\toffline\n\
                   GHI789\tno permissions\n\
                   * daemon started successfully *\n";
        let devices = parse_device_lines(out);
        assert_eq!(
            devices,
            vec![
                "emulator-5554".to_string(),
                "ABC123 (unauthorized)".to_string(),
                "DEF456 (offline)".to_string(),
                "GHI789 (no permissions)".to_string(),
            ],
            "a ready device is bare; others are labeled and none are dropped"
        );
    }

    #[test]
    fn device_serial_strips_state_label() {
        assert_eq!(device_serial("emulator-5554"), "emulator-5554");
        assert_eq!(device_serial("ABC123 (unauthorized)"), "ABC123");
        assert_eq!(device_serial("DEF456 (offline)"), "DEF456");
    }

    #[test]
    fn parse_hdc_targets_lists_keys_and_handles_empty() {
        // Real hdc emits CRLF line endings and prints `[Empty]` with no device.
        assert_eq!(parse_hdc_targets("[Empty]\r\n"), Vec::<String>::new());
        assert_eq!(
            parse_hdc_targets("key123\r\nabc-def\r\n"),
            vec!["key123".to_string(), "abc-def".to_string()]
        );
        assert_eq!(parse_hdc_targets("  \n key1 \n"), vec!["key1".to_string()]);
    }

    /// End-to-end check against a real hdc binary. Ignored by default; run with:
    /// `HDC_BIN=/path/to/hdc LD_LIBRARY_PATH=/path/to/hdc-libs \
    ///  cargo test -- --ignored real_hdc_list_devices --nocapture`
    /// With no device connected hdc prints `[Empty]`, so the parsed list is empty.
    #[test]
    #[ignore = "requires a real hdc binary; set HDC_BIN"]
    fn real_hdc_list_devices() {
        let Ok(bin) = std::env::var("HDC_BIN") else {
            return;
        };
        let devices = list_devices(Transport::Hdc, Some(&bin)).expect("`hdc list targets` runs");
        eprintln!("real hdc reported devices: {devices:?}");
    }

    /// Full data-path check against a real HarmonyOS device: start a `hilog`
    /// session through [`Session::start`], read what streams in, and confirm the
    /// lines parse as HiLog. Ignored by default; run with:
    /// `HDC_BIN=/path/to/hdc LD_LIBRARY_PATH=/path/to/hdc-libs \
    ///  cargo test -- --ignored real_hdc_streams_and_parses_hilog --nocapture`
    #[test]
    #[ignore = "requires a real hdc binary + connected HarmonyOS device; set HDC_BIN"]
    fn real_hdc_streams_and_parses_hilog() {
        use std::time::{Duration, Instant};
        let Ok(bin) = std::env::var("HDC_BIN") else {
            return;
        };
        let devices = list_devices(Transport::Hdc, Some(&bin)).expect("list targets");
        let Some(dev) = devices.first() else {
            eprintln!("no HarmonyOS device connected — skipping stream check");
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(8192);
        let mut session = Session::start(
            Transport::Hdc,
            Some(&bin),
            Some(device_serial(dev)),
            "hilog -v time",
            tx,
            1,
        )
        .expect("hilog session starts");

        let mut lines = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline && lines.len() < 200 {
            if let Ok((_, line)) = rx.recv_timeout(Duration::from_millis(500)) {
                lines.push(line);
            }
        }
        session.stop();

        assert!(!lines.is_empty(), "expected hilog lines from the device");
        let hilog = lines
            .iter()
            .filter(|l| crate::parser::parse_line((*l).clone()).1 == LogFormat::HiLog)
            .count();
        eprintln!(
            "hilog stream: {hilog}/{} lines parsed as HiLog",
            lines.len()
        );
        assert!(
            hilog * 2 >= lines.len(),
            "majority of streamed lines should parse as HiLog"
        );
    }

    #[test]
    fn transport_binary_list_args_and_device_flag() {
        assert_eq!(Transport::Adb.binary(), "adb");
        assert_eq!(Transport::Hdc.binary(), "hdc");
        assert_eq!(Transport::Adb.device_flag(), "-s");
        assert_eq!(Transport::Hdc.device_flag(), "-t");
        assert_eq!(Transport::Adb.list_args(), ["devices"]);
        assert_eq!(Transport::Hdc.list_args(), ["list", "targets"]);
    }

    /// A configured binary path must be honoured or reported, never silently
    /// replaced by whatever `adb` happens to be on PATH: that would be a
    /// different server with a different set of visible devices, and a typo
    /// would present as "no devices found".
    #[test]
    fn a_bad_binary_override_is_reported_instead_of_falling_back_to_path() {
        // No override: the bare name, resolved against PATH by Command.
        assert_eq!(
            binary_path(Transport::Adb, None).unwrap(),
            PathBuf::from("adb")
        );

        // A real file is used as given.
        assert_eq!(
            binary_path(Transport::Adb, Some("/bin/sh")).unwrap(),
            PathBuf::from("/bin/sh")
        );

        for bad in ["/nonexistent/path/to/adb", "", "/tmp"] {
            let err = binary_path(Transport::Adb, Some(bad))
                .expect_err("a non-file override must be an error");
            assert!(
                err.to_string().contains("not a runnable file"),
                "unexpected error for {bad:?}: {err}"
            );
        }
    }

    /// `shlex::split` returns None exactly when the command is not valid shell
    /// syntax. The old whitespace-split fallback passed the literal quote
    /// through as argv, so `-s "My Tag` filtered on the tag `"My` and the user
    /// saw only "session ended".
    #[test]
    fn an_unbalanced_quote_in_the_command_is_refused() {
        let (tx, _rx) = log_channel();
        let result = Session::start(
            Transport::Adb,
            Some("/bin/sh"),
            None,
            "-c 'logcat -s \"My Tag",
            tx,
            1,
        );
        match result {
            Ok(_) => panic!("an unbalanced quote must not be split on whitespace"),
            Err(e) => assert!(
                e.to_string().contains("unbalanced quotes"),
                "unexpected error: {e}"
            ),
        }
    }

    #[test]
    fn default_commands_split_by_backend() {
        let adb = default_commands(Transport::Adb);
        let hdc = default_commands(Transport::Hdc);
        // logcat + kmsg are adb-only; hilog is hdc-only; no overlap.
        assert!(adb
            .iter()
            .all(|c| c.starts_with("logcat") || c.starts_with("shell")));
        assert!(hdc.iter().all(|c| c.starts_with("hilog")));
        assert!(!adb.is_empty() && !hdc.is_empty());
        assert!(adb.iter().all(|c| !hdc.contains(c)));
        // Kernel-log command sits last in the adb list.
        assert_eq!(adb.last().map(String::as_str), Some("shell cat /proc/kmsg"));
    }

    #[test]
    fn stop_hands_off_stdout_and_stderr_cleanup() {
        let (tx, rx) = crossbeam_channel::bounded(4);
        let mut session = Session::start(
            Transport::Adb,
            Some("/bin/sh"),
            None,
            "-c 'printf output; printf error >&2'",
            tx,
            1,
        )
        .expect("shell-backed session should start");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !session.stdout_ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.stdout_ended(),
            "stdout worker should observe process exit"
        );

        session.stop();

        assert_eq!(rx.try_recv(), Ok((1, "output".into())));
        wait_until(|| session.stderr_text() == "error");
        assert_eq!(session.stderr_text(), "error");
        assert!(session.child.is_none());
        assert!(session.reader_handle.is_none());
        assert!(session.stderr_handle.is_none());
        assert!(session.reader_thread.is_none());
    }

    #[test]
    fn session_epoch_changes_only_future_lines_without_restarting_reader() {
        let gate = std::env::temp_dir().join(format!(
            "lf_epoch_gate_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!(
            "printf 'before\\n'; while [ ! -e {} ]; do sleep 0.01; done; printf 'after\\n'",
            shlex::try_quote(gate.to_str().unwrap()).unwrap()
        );
        let cmd = format!("-c {}", shlex::try_quote(&script).unwrap());
        let (tx, rx) = log_channel();
        let mut session =
            Session::start(Transport::Adb, Some("/bin/sh"), None, &cmd, tx, 1).unwrap();
        let timeout = std::time::Duration::from_secs(5);
        // Leave the first line queued across the epoch change.
        let deadline = std::time::Instant::now() + timeout;
        while rx.is_empty() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        session.set_epoch(2);
        std::fs::write(&gate, b"").unwrap();
        assert_eq!(rx.recv_timeout(timeout).unwrap(), (1, "before".into()));
        assert_eq!(rx.recv_timeout(timeout).unwrap(), (2, "after".into()));
        wait_for_reap(&mut session);
        std::fs::remove_file(gate).unwrap();
    }

    #[test]
    fn try_reap_waits_for_complete_stderr_of_a_self_ended_session() {
        let (tx, _rx) = crossbeam_channel::bounded(4);
        let mut session = Session::start(
            Transport::Adb,
            Some("/bin/sh"),
            None,
            "-c 'printf \"device offline\" >&2'",
            tx,
            1,
        )
        .expect("shell-backed session should start");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !session.stdout_ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.stdout_ended(), "stdout reader should finish");

        wait_for_reap(&mut session);
        assert_eq!(
            session.stderr_text(),
            "device offline",
            "reap must join the stderr worker so the reason is complete"
        );
        assert!(session.stderr_handle.is_none(), "stderr worker joined");
        assert!(session.reader_handle.is_none(), "stdout worker joined");
    }

    #[test]
    fn stdout_eof_does_not_block_on_a_running_child_or_lose_late_stderr() {
        let gate = gate_path("stdout_eof");
        let script = format!(
            "exec 1>&-; while [ ! -e {} ]; do sleep 0.01; done; printf 'late error' >&2",
            shlex::try_quote(gate.to_str().unwrap()).unwrap()
        );
        let cmd = format!("-c {}", shlex::try_quote(&script).unwrap());
        let (tx, _rx) = log_channel();
        let mut session =
            Session::start(Transport::Adb, Some("/bin/sh"), None, &cmd, tx, 1).unwrap();
        wait_until(|| session.stdout_ended());
        assert!(session
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none());

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            let done = session.try_reap().unwrap();
            done_tx.send((done, session)).unwrap();
        });
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        // Release even on failure, so a regressed blocking wait can unwind.
        std::fs::write(&gate, b"").unwrap();
        let (done, mut session) = result.expect("try_reap blocked waiting for child exit");
        assert!(!done, "stdout EOF must not be treated as process exit");
        worker.join().unwrap();
        wait_for_reap(&mut session);
        assert_eq!(session.stderr_text(), "late error");
        assert!(session.child.is_none());
        assert!(session.try_reap().unwrap(), "completion is idempotent");
        std::fs::remove_file(gate).unwrap();
    }

    #[test]
    fn inherited_stderr_does_not_block_poll_stop_or_drop() {
        for drop_session in [false, true] {
            let gate = gate_path("inherited_stderr");
            let script = format!(
                "exec 1>&-; (while [ ! -e {} ]; do sleep 0.01; done; printf 'descendant error' >&2) & exit 0",
                shlex::try_quote(gate.to_str().unwrap()).unwrap());
            let cmd = format!("-c {}", shlex::try_quote(&script).unwrap());
            let (tx, _rx) = log_channel();
            let mut session =
                Session::start(Transport::Adb, Some("/bin/sh"), None, &cmd, tx, 1).unwrap();
            wait_until(|| session.stdout_ended());
            wait_until(|| {
                session
                    .child
                    .as_mut()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_some()
            });
            let stderr = session.stderr.clone();
            let (done_tx, done_rx) = crossbeam_channel::bounded(1);
            let worker = thread::spawn(move || {
                // The direct child has exited, but joining stderr would block.
                assert!(!session.try_reap().unwrap());
                if !drop_session {
                    session.stop();
                    session.stop();
                }
                drop(session);
                done_tx.send(()).unwrap();
            });
            let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            std::fs::write(&gate, b"").unwrap();
            result.expect("pipe cleanup blocked the caller");
            worker.join().unwrap();
            wait_until(|| stderr.lock().unwrap().as_str() == "descendant error");
            std::fs::remove_file(gate).unwrap();
        }
    }

    /// A descendant holding stderr open must not strand the session.
    ///
    /// Completion used to be gated on the stderr worker reaching EOF, so a
    /// surviving descendant kept `try_reap` returning `false` forever: the UI
    /// never saw the capture end, and the worker plus its 256 KiB buffer were
    /// never released. Diagnostics written before the hang must also already be
    /// readable, since they used to be published only at EOF.
    #[test]
    fn a_descendant_holding_stderr_cannot_strand_a_finished_session() {
        let gate = gate_path("stderr_grace");
        let script = format!(
            "exec 1>&-; printf 'early error' >&2; \
             (while [ ! -e {} ]; do sleep 0.01; done) & exit 0",
            shlex::try_quote(gate.to_str().unwrap()).unwrap()
        );
        let cmd = format!("-c {}", shlex::try_quote(&script).unwrap());
        let (tx, _rx) = log_channel();
        let mut session =
            Session::start(Transport::Adb, Some("/bin/sh"), None, &cmd, tx, 1).unwrap();
        wait_until(|| session.stdout_ended());
        wait_until(|| {
            session
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_some()
        });

        // Published while the pipe is still held open by the descendant.
        wait_until(|| session.stderr_text() == "early error");
        // The first check only starts the grace period.
        assert!(
            !session.try_reap().unwrap(),
            "completion should not be declared before the grace period"
        );
        assert!(
            session.stderr_handle.is_some(),
            "the stderr worker is still draining the inherited pipe"
        );

        // And completion arrives anyway, without waiting on that pipe.
        wait_for_reap(&mut session);
        assert_eq!(
            session.stderr_text(),
            "early error",
            "diagnostics survive the handoff"
        );
        assert!(session.child.is_none(), "session reports itself finished");

        std::fs::write(&gate, b"").unwrap();
        std::fs::remove_file(gate).unwrap();
    }

    /// A `Read` that replays scripted steps, so a mid-stream I/O error or a
    /// signal-induced `Interrupted` can be injected without a real device.
    enum Step {
        Data(&'static [u8]),
        Err(std::io::ErrorKind),
    }

    struct ScriptedRead {
        steps: Vec<Step>,
        next: usize,
        /// Bytes of the current step not yet handed over.
        pending: &'static [u8],
    }

    impl ScriptedRead {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps,
                next: 0,
                pending: &[],
            }
        }
    }

    impl std::io::Read for ScriptedRead {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                let Some(step) = self.steps.get(self.next) else {
                    return Ok(0);
                };
                self.next += 1;
                match step {
                    Step::Data(d) => self.pending = d,
                    Step::Err(kind) => return Err(std::io::Error::from(*kind)),
                }
            }
            let n = self.pending.len().min(out.len());
            out[..n].copy_from_slice(&self.pending[..n]);
            self.pending = &self.pending[n..];
            Ok(n)
        }
    }

    fn collect(reader: ScriptedRead) -> (std::io::Result<()>, Vec<String>) {
        let mut got = Vec::new();
        let outcome = stream_lossy_lines(std::io::BufReader::new(reader), |line| {
            got.push(line);
            true
        });
        (outcome, got)
    }

    #[test]
    fn stream_lossy_lines_surfaces_read_errors_instead_of_ending_quietly() {
        // A genuine I/O failure must not look like a normal end of stream: the
        // caller has to be able to report "capture failed" rather than "ended".
        let (outcome, got) = collect(ScriptedRead::new(vec![
            Step::Data(b"first\n"),
            Step::Err(std::io::ErrorKind::BrokenPipe),
        ]));
        let err = outcome.expect_err("an I/O error must be returned, not swallowed");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(got, vec!["first"], "lines before the failure are kept");
    }

    #[test]
    fn stream_lossy_lines_ends_cleanly_at_eof() {
        let (outcome, got) = collect(ScriptedRead::new(vec![Step::Data(b"only\n")]));
        outcome.expect("EOF is a normal end of stream");
        assert_eq!(got, vec!["only"]);
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
        })
        .expect("reading from a Cursor cannot fail");
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
        })
        .expect("reading from a Cursor cannot fail");
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
        })
        .expect("reading from a Cursor cannot fail");
        // Returning at all (no hang/panic) is the assertion.
    }

    #[test]
    fn log_channel_never_blocks_a_producer_that_is_not_drained() {
        // Nobody receives. A bounded channel would block (or time out) once its
        // capacity was reached, which is exactly the pipe backup this prevents.
        let (tx, _rx) = log_channel();
        for i in 0..100_000 {
            assert!(
                tx.send_timeout(
                    (1u64, format!("line {i}")),
                    std::time::Duration::from_secs(5)
                )
                .is_ok(),
                "the log channel must never block the device reader"
            );
        }
    }

    #[test]
    fn the_reader_never_blocks_and_loses_nothing_when_nobody_drains() {
        // 20k lines with nothing consuming them. A bounded channel would block the
        // reader part-way through, so it would never reach EOF; the unbounded one
        // buffers them all and loses none.
        let (tx, rx) = log_channel();
        let mut session = Session::start(
            Transport::Adb,
            Some("/bin/sh"),
            None,
            "-c 'yes line | head -n 20000'",
            tx,
            1,
        )
        .expect("shell-backed session should start");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !session.stdout_ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.stdout_ended(),
            "the reader must not wait for the log channel"
        );
        wait_for_reap(&mut session);

        let lines: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert_eq!(lines.len(), 20_000, "no line may be lost");
        assert!(lines.iter().all(|l| l == "line"), "content must be intact");
    }

    #[test]
    fn paused_capture_drops_lines_but_still_drains_the_stream() {
        // Pause before the child emits: every line is dropped, and the reader must
        // still run to EOF. Stopping the read instead would back adb's pipe up and
        // make the device's logd rotate our logs away.
        let (tx, rx) = crossbeam_channel::bounded(16);
        let mut session = Session::start(
            Transport::Adb,
            Some("/bin/sh"),
            None,
            "-c 'sleep 0.1; printf \"a\\nb\\nc\\n\"'",
            tx,
            1,
        )
        .expect("shell-backed session should start");
        session.set_paused(true);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !session.stdout_ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.stdout_ended(),
            "a paused reader must still drain to EOF, not stall the pipe"
        );
        wait_for_reap(&mut session);

        let lines: Vec<String> = rx.try_iter().map(|(_, l)| l).collect();
        assert!(
            lines.is_empty(),
            "paused capture must not record the pause window, got {lines:?}"
        );
    }
}
