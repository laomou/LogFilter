# LogFilter

[中文](README_zh.md)

LogFilter is a desktop Android logcat viewer and filter written in Rust with egui. It is a Rust port of the original Java Swing LogFilter, focused on opening large log files, streaming adb output, and quickly narrowing logs by level, process, thread, tag, and message text.

## Features

- Open local log files, including drag-and-drop and recent-file history.
- Stream logs from adb with selectable device and command presets.
- Parse common Android log formats: `threadtime`, `time`, `brief`, and kernel-style lines.
- Filter by log level, PID, TID, tag, message text, bookmarks, and errors.
- Highlight matching words separately from filtering.
- Toggle bookmarks by double-clicking rows and navigate via the right-side indicator.
- Save the currently filtered result to a timestamped text file.
- Customize visible columns, font size, table font, language, colors, encoding, and adb command presets.
- Supports English and Chinese UI text.

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
