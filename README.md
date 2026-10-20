# LogFilter

[![crates.io](https://img.shields.io/crates/v/logfilter.svg?logo=rust)](https://crates.io/crates/logfilter)
[![CI](https://github.com/laomou/LogFilter/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/LogFilter/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[中文](README_zh.md)

LogFilter is a desktop viewer and filter for Android (logcat via adb) and HarmonyOS (hilog via hdc) logs, written in Rust with egui, focused on opening large log files, streaming device output, and quickly narrowing logs by level, process, thread, tag, and message text.

## Features

- Open local log files, including drag-and-drop and recent-file history.
- Browse large UTF-8 logs with an estimated-memory switch, a sparse disk index,
  and RAM-adaptive pages, without loading the complete file into memory.
- Stream logs from a device over adb (Android) or hdc (HarmonyOS), with selectable device and command presets.
- Automatically record original device output to disk, with a bounded live preview cache.
- Parse common Android logcat formats (`threadtime`, `time`, `brief`, kernel) and HarmonyOS `hilog`.
- Filter by log level, PID, TID, tag, message text, bookmarks, and errors.
- Highlight matching words separately from filtering.
- Toggle bookmarks by double-clicking rows and navigate via the right-side indicator.
- Save the currently filtered result to a timestamped text file.
- Customize visible columns, font size, table font, language, colors, encoding, and device command presets.
- Supports English and Chinese UI text.

## Device recording and exports

Each Run/Restart creates a persistent, timestamped `.log` file in `logs/` under
the app's per-user data directory (for example `~/.local/share/logfilter/logs` on
Linux, `%LOCALAPPDATA%\logfilter\logs` on Windows). Files are not temporary and
are not deleted on Clear or application exit. The GUI shows both the directory
and current full file path.

The reader writes original stdout bytes to disk before the preview reads them.
Rendering and filtering do not hold up the writer. Pause keeps draining the
device pipe but does not save logs received during the pause. Clear starts
the preview at the current recording position and preserves the entire file.
Older preview rows (and their in-memory bookmarks) are evicted at the cache budget.
The disk backlog is displayed when the preview is behind. Lines larger than 1 MiB
are retained in the raw file but omitted from the preview with a visible count.

The File menu provides **Export filtered results**, which scans the full recorded
snapshot using the current filter settings, including rows evicted from the preview.
Output is normalized UTF-8 text. Individual decoded lines over 64 MiB cause a
reported export error; the original `.log` remains intact. Bookmarks exist only
in the current preview, so bookmarks-only mode exports the cached bookmarked rows.
Ctrl/Cmd+S invokes the same export behavior.

Exports run in the background with bounded buffers. A filter change or continued
recording does not alter an export's snapshot. Write, flush and sync failures stop
the recording and report the error; a partially written file is retained. Stop
finalizes in the background, and normal window close waits for final synchronization.
Periodic synchronization occurs during writes; sudden power loss can still lose
data not yet synchronized. Disk capacity and write throughput remain finite.

Capture keeps a 1 GiB free-space reserve on the disk by default and uses an
automatic memory preview budget. When the reserve is reached, capture stops
with an error and existing files remain intact. LogFilter does not silently
delete or overwrite old recordings.

Optional configuration:

```toml
[recording]
cache_mib = 0 # automatic; set e.g. 2048 for a larger preview cache
min_free_mib = 1024
```

The automatic cache budget is one fifth of the existing log-memory budget
(roughly 10% of available RAM where the platform reports it), capped at 2 GiB.
An explicit cache setting is constrained by the total log-memory budget.

## Install

Install the latest release with Cargo (needs a Rust toolchain):

```sh
cargo install logfilter
```

Or download a prebuilt binary for Windows / Linux / macOS from the [releases page](https://github.com/laomou/LogFilter/releases).

## Build

Install a recent Rust toolchain, then run:

```sh
cargo build
```

For an optimized binary:

```sh
cargo build --release
```

The release executable is written under `target/release/`.

## Run

Start the application:

```sh
cargo run
```

Open a log file at startup:

```sh
cargo run -- path/to/log.txt
```

You can also open files from the File menu or drop a file onto the window.

### Large-file paging

Files under 256 MiB load normally. From 256 MiB to 4 GiB, LogFilter samples the
first 8 MiB and estimates full parsed memory from raw bytes, row count, per-row
structure/index overhead, and a 25% safety margin. It enters paging when that
estimate exceeds 60% of the log-memory budget. Files at least 4 GiB always page.

Paged mode scans the file in the background and stores a byte-offset checkpoint
for each page. The parsed-row budget is half of the log-memory budget, clamped
to 32 MiB–8 GiB. Sampling selects a row cap of 20,000–5,000,000, but each page
also obeys a conservative byte budget, including invalid UTF-8 expansion and
row/index overhead. Long rows therefore produce smaller pages without skipping
source lines. Index memory grows with page count, not individual row count.
Pages become available progressively; Goto locates the page by its source-line
checkpoint. Superseded reads are cancelled, and only one page read per file
allocates at a time, alongside the currently displayed page.

Find, remove, level, PID, TID, and tag filters scan the complete file in the
background and build a separate sparse match index. Filtered pages preserve the
original source line numbers. Goto uses the source line number when no global
filter is active. Filtered export streams the complete file with a snapshot of
the current filters. Bookmarks remain scoped to the loaded page.
Column-picker counts and bookmark markers describe the loaded page; selecting a
column value still applies that value as a global disk filter.

Paged mode currently supports UTF-8. Files are treated as a snapshot: if they
change while indexed, reopen them. Page reads and exports validate the original
snapshot before and after reading; failed exports remove their partial output.
Source lines larger than 64 MiB, or too large for the page budget, produce an
explicit error. The budget estimates parsed data rather than total process RSS;
rendering, indexing, and bounded I/O buffers also use memory.

## adb Streaming

LogFilter can launch adb commands from the toolbar. By default it includes presets such as:

- `logcat -v threadtime`
- `logcat -v time`
- `logcat -b radio -v time`
- `logcat -b events -v time`
- `shell cat /proc/kmsg`

If `adb` is not on `PATH`, set `adb.adb_path` in the config file. On Windows, the app also checks the default Android Studio SDK location.

## Filtering

The main Find, Remove, and Highlight fields accept `|` separated terms. Matching is case-insensitive.

- Find: keep rows whose message contains any term.
- Remove: exclude rows whose message contains any term.
- Highlight: visually mark matching text without changing the filtered row set.
- Column headers for level, PID, thread, and tag can open picker panels for value-based filtering.
- Alt-click a tag cell to show only that tag; Alt-right-click a tag cell to exclude it.

## Configuration

Configuration is stored using the platform config directory:

- Linux: `~/.config/logfilter/config.toml`
- Windows: `%APPDATA%/logfilter/config/config.toml`
- macOS: `~/Library/Application Support/logfilter/config.toml`

Custom fonts can be dropped into the `fonts` subdirectory under the same config directory. The app loads `.ttf`, `.otf`, `.ttc`, and `.otc` files at startup and exposes them in the Format > Font menu.

## Development

Run the test suite:

```sh
cargo test
```

Check formatting:

```sh
cargo fmt --check
```

Enable tracing logs with `RUST_LOG`, for example:

```sh
RUST_LOG=logfilter=debug cargo run
```

## License

Apache-2.0, see [LICENSE](LICENSE).
