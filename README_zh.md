# LogFilter

[English](README.md)

LogFilter 是一款用 Rust 和 egui 开发的桌面端 Android logcat 查看与过滤工具，专注于打开大型日志文件、流式显示 adb 输出，以及按级别、进程、线程、标签和消息文本快速筛选日志。

## 功能特性

- 打开本地日志文件，支持拖放和最近文件历史。
- 通过 adb 流式获取日志，支持选择设备和命令预设。
- 解析常见 Android 日志格式：`threadtime`、`time`、`brief` 以及内核格式。
- 按日志级别、PID、TID、标签、消息文本、书签和错误进行过滤。
- 独立于过滤条件的高亮关键词标记。
- 双击行切换书签，通过右侧指示器快速导航。
- 将当前过滤结果保存为带时间戳的文本文件。
- 自定义可见列、字体大小、表格字体、界面语言、颜色、编码和 adb 命令预设。
- 支持英文和中文界面。

## 构建

安装最新的 Rust 工具链，然后运行：

```sh
cargo build
```

构建优化的发布版本：

```sh
cargo build --release
```

可执行文件位于 `target/release/` 目录下。

## 运行

启动应用：

```sh
cargo run
```

启动时直接打开日志文件：

```sh
cargo run -- 路径/到/日志.txt
```

你也可以通过文件菜单打开文件，或将文件拖放到窗口上。

## adb 流式日志

LogFilter 可以从工具栏直接启动 adb 命令。默认包含以下预设命令：

- `logcat -v threadtime`
- `logcat -v time`
- `logcat -b radio -v time`
- `logcat -b events -v time`
- `shell cat /proc/kmsg`

如果 `adb` 不在 `PATH` 环境变量中，可以在配置文件中设置 `adb.adb_path`。在 Windows 上，应用还会检查默认的 Android Studio SDK 路径。

## 过滤功能

主要的查找、排除和高亮输入框支持以 `|` 分隔的多个关键词。匹配时忽略大小写。

- **查找**：保留消息中包含任一关键词的行。
- **排除**：排除消息中包含任一关键词的行。
- **高亮**：在视觉上标记匹配的文本，不影响过滤结果集。
- 级别、PID、线程和标签的列头可以打开选择器面板，进行基于值的过滤。
- 按住 Alt 点击标签单元格可以仅显示该标签；按住 Alt 右键点击标签单元格可以排除该标签。

## 配置

配置文件保存在平台对应的配置目录中：

- **Linux**：`~/.config/logfilter/config.toml`
- **Windows**：`%APPDATA%/logfilter/config/config.toml`
- **macOS**：`~/Library/Application Support/logfilter/config.toml`

自定义字体可以放入配置目录下的 `fonts` 子目录。应用在启动时加载 `.ttf`、`.otf`、`.ttc` 和 `.otc` 文件，并在 格式 > 字体 菜单中列出。

## 开发

运行测试套件：

```sh
cargo test
```

检查代码格式：

```sh
cargo fmt --check
```

使用 `RUST_LOG` 环境变量启用追踪日志，例如：

```sh
RUST_LOG=logfilter=debug cargo run
```

## 许可协议

MIT
