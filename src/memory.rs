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
const MIN_BUDGET: u64 = 256 * 1024 * 1024;

/// Ceiling. Without one, a huge host would let the log grow until the machine
/// starts swapping — exactly the stall this budget exists to avoid.
const MAX_BUDGET: u64 = 32 * 1024 * 1024 * 1024;

/// Used when the platform cannot report memory.
const FALLBACK_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// Bytes of system memory available to new allocations right now, if the platform
/// can tell us.
#[cfg(target_os = "linux")]
fn platform_available() -> Option<u64> {
    // `MemAvailable` is the kernel's own estimate of what a new workload can claim
    // without swapping. Present since Linux 3.14.
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    })
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reports_available_memory() {
        let available = platform_available().expect("MemAvailable should be readable here");
        assert!(available > 0);
        assert!(log_budget() >= MIN_BUDGET);
    }
}
