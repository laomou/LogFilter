# LogFilter

[![crates.io](https://img.shields.io/crates/v/logfilter.svg?logo=rust)](https://crates.io/crates/logfilter)
[![CI](https://github.com/laomou/LogFilter/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/LogFilter/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[English](README.md)

LogFilter 是一款用 Rust 和 egui 开发的桌面端日志查看与过滤工具，支持 Android（通过 adb 的 logcat）与 HarmonyOS（通过 hdc 的 hilog），专注于打开大型日志文件、流式显示设备输出，以及按级别、进程、线程、标签和消息文本快速筛选日志。

## 功能特性

- 打开本地日志文件，支持拖放和最近文件历史。
- 通过 adb（Android）或 hdc（HarmonyOS）流式获取设备日志，支持选择设备和命令预设。
- 设备原始输出自动录制到磁盘，界面只保留有容量限制的预览缓存。
- 解析常见 Android logcat 格式（`threadtime`、`time`、`brief`、内核）以及 HarmonyOS `hilog`。
- 按日志级别、PID、TID、标签、消息文本、书签和错误进行过滤。
- 独立于过滤条件的高亮关键词标记。
- 双击行切换书签，通过右侧指示器快速导航。
- 将当前过滤结果保存为带时间戳的文本文件。
- 自定义可见列、字体大小、表格字体、界面语言、颜色、编码和设备命令预设。
- 支持英文和中文界面。

## 设备录制与文件导出

每次运行或重启采集都会在当前工作目录的 `logs/` 下创建新的时间戳 `.log` 文件。
这是持久文件，不是临时文件；清空预览和退出程序都不会删除它。GUI 同时显示
`logs` 目录和当前文件的完整路径。

原始 stdout 字节先写入磁盘，预览再从文件读取。暂停预览、过滤和绘制不会阻塞
录制线程。Clear 从当前录制位置重新开始预览，完整录制文件保持不变。缓存到达
预算后淘汰较早的预览行及其内存书签；预览落后时，积压保留在磁盘并显示积压大小。
超过 1 MiB 的单行仅从预览中略去并显示计数，原始录制仍保留完整内容。

“文件”菜单提供 **导出过滤结果**：按当前过滤条件快照扫描全部录制历史，包含已经
淘汰的预览行，输出规范化的 UTF-8 文本。单行解码后超过 64 MiB 会报告导出失败，
原始 `.log` 文件仍保持完整。书签只存在于当前预览中，因此“仅书签”模式导出当前
缓存里的书签行。Ctrl/Cmd+S 使用相同逻辑。

导出在后台使用有限缓冲区完成，后续追加日志或修改过滤条件不会改变本次快照。
磁盘写入、flush 或同步失败会停止录制并显示错误，已写入的部分文件会保留。
Stop 在后台完成收尾，正常退出会等待录制文件同步完成。写入过程中周期同步，
突然断电仍可能丢失尚未同步的数据；磁盘容量和写入速度也有上限。

GUI 提供缓存设置。默认限制 `logs/` 中全部文件合计不超过 10 GiB，并至少保留
1 GiB 磁盘空间；达到任一保护条件都会停止采集并报错，已有文件保持不变。
LogFilter 不会静默删除或覆盖历史录制文件。

也可以在配置文件中设置：

```toml
[recording]
cache_mib = 0 # 自动；也可设置为 2048 等数值
max_disk_mib = 10240
min_free_mib = 1024
```

自动缓存预算取现有日志内存预算的五分之一（能查询可用内存的平台上约为可用内存
的 10%），默认最高 2 GiB；手动设置仍受总日志内存预算限制。

## 安装

用 Cargo 安装最新发布版（需要 Rust 工具链）：

```sh
cargo install logfilter
```

或从 [releases 页面](https://github.com/laomou/LogFilter/releases) 下载 Windows / Linux / macOS 预编译包。

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
