//! How much of the machine's memory the in-memory log may occupy.
//!
//! Local-file loading stops adding rows at its budget. Live device recording
//! writes directly to disk; its smaller preview cache evicts old rows without
//! affecting the raw recording. These are estimates of model memory, not RSS.

/// Divisor applied to the machine's available memory.
///
/// Half leaves room for the OS, other applications and the app's own non-model
/// allocations (UI, filter bitmap, export buffers) while still letting a big
/// machine capture far more than a small one.
const BUDGET_DIVISOR: u64 = 2;

/// Floor, so a machine (or container) that reports very little still gets a
/// usable window.
///
/// Deliberately small: with cgroup limits taken into account (see
/// [`platform_available`]) a container may legitimately report only a few
/// hundred MiB, and a floor larger than half of that would plan to hold more
/// memory than the container is allowed to use.
const MIN_BUDGET: u64 = 64 * 1024 * 1024;

/// Ceiling. Without one, a huge host would let the log grow until the machine
/// starts swapping — exactly the stall this budget exists to avoid.
const MAX_BUDGET: u64 = 32 * 1024 * 1024 * 1024;

/// Used when the platform cannot report memory.
const FALLBACK_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// Bytes of system memory available to new allocations right now, if the platform
/// can tell us.
///
/// On Linux this is the smallest of what the host reports as available, the
/// host's total memory, and the headroom left inside a cgroup limit. Reading
/// only `/proc/meminfo` was wrong inside Docker/k8s/containers: `MemAvailable`
/// describes the *host*, so a 512 MiB container on a 64 GiB machine planned a
/// multi-gigabyte log and was OOM-killed instead of self-limiting.
#[cfg(target_os = "linux")]
fn platform_available() -> Option<u64> {
    combine_limits(
        meminfo_kib("MemAvailable"),
        meminfo_kib("MemTotal"),
        cgroup_available(),
    )
}

/// Smallest of the candidate limits; `None` only when no source reported one.
fn combine_limits(
    mem_available: Option<u64>,
    mem_total: Option<u64>,
    cgroup_available: Option<u64>,
) -> Option<u64> {
    [mem_available, mem_total, cgroup_available]
        .into_iter()
        .flatten()
        .min()
}

/// One `/proc/meminfo` field, in bytes.
fn meminfo_kib(field: &str) -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_kib(&meminfo, field).map(|kib| kib.saturating_mul(1024))
}

/// One `/proc/meminfo` field's value in KiB (the file's unit).
fn parse_meminfo_kib(meminfo: &str, field: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// Headroom left inside the process's cgroup memory limit, if one applies.
///
/// cgroup v2 exposes the limit and usage as separate files; v1 keeps them under
/// `memory/`. Both spell "unlimited" differently (v2: `max`, v1: a huge
/// page-aligned number), so [`parse_cgroup_value`] normalises that to `None`.
#[cfg(target_os = "linux")]
fn cgroup_available() -> Option<u64> {
    let (limit, usage) = if std::path::Path::new("/sys/fs/cgroup/memory.max").exists() {
        ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.current")
    } else {
        (
            "/sys/fs/cgroup/memory/memory.limit_in_bytes",
            "/sys/fs/cgroup/memory/memory.usage_in_bytes",
        )
    };
    let limit = parse_cgroup_value(&std::fs::read_to_string(limit).ok()?)?;
    let usage = parse_cgroup_value(&std::fs::read_to_string(usage).ok()?).unwrap_or(0);
    Some(limit.saturating_sub(usage))
}

/// A cgroup memory value in bytes; `None` means "no limit".
#[cfg(target_os = "linux")]
fn parse_cgroup_value(text: &str) -> Option<u64> {
    let text = text.trim();
    if text == "max" {
        return None;
    }
    let value: u64 = text.parse().ok()?;
    // cgroup v1 reports "unlimited" as a huge page-aligned count; treat anything
    // that large as no limit rather than as a real cap.
    (value < (1 << 62)).then_some(value)
}

#[cfg(windows)]
fn platform_available() -> Option<u64> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MaybeUninit::<MEMORYSTATUSEX>::zeroed();
    // SAFETY: `status` is valid writable storage for a `MEMORYSTATUSEX`, and
    // `dwLength` is set to its size as the API requires before the call.
    unsafe {
        let ptr = status.as_mut_ptr();
        (*ptr).dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(ptr) != 0 {
            Some((*ptr).ullAvailPhys)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn platform_available() -> Option<u64> {
    // No portable way to ask; `budget_from` falls back to a fixed default.
    None
}

/// Resident budget for the in-memory log, clamped so it is always usable and never
/// so large that the machine swaps before we stop.
fn budget_from(available: Option<u64>) -> u64 {
    match available {
        Some(available) => (available / BUDGET_DIVISOR).clamp(MIN_BUDGET, MAX_BUDGET),
        None => FALLBACK_BUDGET,
    }
}

/// Resident budget for the in-memory log on this machine, sampled once at startup.
pub fn log_budget() -> u64 {
    budget_from(platform_available())
}

/// Preview is a cache: roughly 10% of initially available memory, capped at
/// 2 GiB by default. Configuration can override it, still within the log budget.
pub fn preview_budget(configured_mib: u64, log_budget: u64) -> u64 {
    let requested = if configured_mib == 0 {
        (log_budget / 5).clamp(32 * 1024 * 1024, 2 * 1024 * 1024 * 1024)
    } else {
        configured_mib.saturating_mul(1024 * 1024)
    };
    requested.clamp(4 * 1024 * 1024, log_budget.max(4 * 1024 * 1024))
}

/// Parsed rows for one large-file page may use more RAM on larger machines,
/// while still leaving half of the overall log budget for the UI and indexes.
pub fn paged_file_budget(log_budget: u64) -> u64 {
    (log_budget / 2).clamp(32 * 1024 * 1024, 8 * 1024 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn budget_is_half_of_available_inside_the_clamp() {
        assert_eq!(budget_from(Some(16 * GIB)), 8 * GIB);
        assert_eq!(budget_from(Some(4 * GIB)), 2 * GIB);
    }

    #[test]
    fn budget_is_clamped_at_both_ends() {
        // A tiny machine or a constrained container still gets a usable window.
        assert_eq!(budget_from(Some(64 * 1024 * 1024)), MIN_BUDGET);
        // A huge host must not be allowed to grow until the machine swaps.
        assert_eq!(budget_from(Some(1024 * GIB)), MAX_BUDGET);
    }

    #[test]
    fn budget_falls_back_when_the_platform_cannot_report_memory() {
        assert_eq!(budget_from(None), FALLBACK_BUDGET);
    }

    #[test]
    fn paged_file_budget_scales_and_is_clamped() {
        assert_eq!(paged_file_budget(2 * GIB), GIB);
        assert_eq!(paged_file_budget(128 * 1024 * 1024), 64 * 1024 * 1024);
        assert_eq!(paged_file_budget(64 * GIB), 8 * GIB);
    }

    #[test]
    fn meminfo_fields_are_parsed_in_kib() {
        let meminfo = "MemTotal:       16316384 kB\nMemFree:         1234567 kB\nMemAvailable:    9876543 kB\n";
        assert_eq!(parse_meminfo_kib(meminfo, "MemAvailable"), Some(9_876_543));
        assert_eq!(parse_meminfo_kib(meminfo, "MemTotal"), Some(16_316_384));
        assert_eq!(parse_meminfo_kib(meminfo, "Missing"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_values_normalise_unlimited() {
        assert_eq!(parse_cgroup_value("max\n"), None, "cgroup v2 unlimited");
        assert_eq!(
            parse_cgroup_value("9223372036854771712\n"),
            None,
            "cgroup v1 unlimited sentinel"
        );
        assert_eq!(parse_cgroup_value("536870912\n"), Some(536_870_912));
        assert_eq!(parse_cgroup_value("garbage"), None);
    }

    #[test]
    fn the_tightest_limit_wins() {
        const MIB: u64 = 1024 * 1024;
        // A container below its host: the cgroup headroom is what matters.
        assert_eq!(
            combine_limits(Some(60_000 * MIB), Some(64_000 * MIB), Some(300 * MIB)),
            Some(300 * MIB)
        );
        // No cgroup limit: host availability, capped by host total.
        assert_eq!(
            combine_limits(Some(1_000 * MIB), Some(800 * MIB), None),
            Some(800 * MIB)
        );
        // Nothing reported → fall back to the fixed default.
        assert_eq!(combine_limits(None, None, None), None);
    }

    #[test]
    fn a_tiny_cgroup_still_gets_a_usable_but_small_budget() {
        // A container with 400 MiB of headroom gets half of it. The old 256 MiB
        // floor would have planned to hold 256 MiB — most of the container.
        assert_eq!(budget_from(Some(400 * 1024 * 1024)), 200 * 1024 * 1024);
        // With even less headroom the floor still guarantees a usable window; at
        // 64 MiB it stays small relative to any real machine or container.
        assert_eq!(budget_from(Some(100 * 1024 * 1024)), MIN_BUDGET);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reports_available_memory() {
        let available = platform_available().expect("MemAvailable should be readable here");
        assert!(available > 0);
        assert!(log_budget() >= MIN_BUDGET);
    }
}
