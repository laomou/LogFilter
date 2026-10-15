# LogFilter

[![crates.io](https://img.shields.io/crates/v/logfilter.svg?logo=rust)](https://crates.io/crates/logfilter)
[![CI](https://github.com/laomou/LogFilter/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/LogFilter/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[中文](README_zh.md)

LogFilter is a desktop viewer and filter for Android (logcat via adb) and HarmonyOS (hilog via hdc) logs, written in Rust with egui, focused on opening large log files, streaming device output, and quickly narrowing logs by level, process, thread, tag, and message text.

## Features

- Open local log files, including drag-and-drop and recent-file history.
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
the current working directory. Files are not temporary and are not deleted on
Clear or application exit. The GUI shows both the directory and current full
file path.

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

The GUI exposes cache settings. Defaults are a 10 GiB limit for all files in
`logs/`, a 1 GiB free-space reserve, and an automatic memory preview budget.
When either disk guard is reached, capture stops with an error and existing files
remain intact. LogFilter does not silently delete or overwrite old recordings.

Optional configuration:

```toml
[recording]
cache_mib = 0 # automatic; set e.g. 2048 for a larger preview cache
max_disk_mib = 10240
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

MIT
