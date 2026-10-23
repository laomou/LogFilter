# LogFilter

[![crates.io](https://img.shields.io/crates/v/logfilter.svg?logo=rust)](https://crates.io/crates/logfilter)
[![CI](https://github.com/laomou/LogFilter/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/LogFilter/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[English](README.md)

LogFilter 是一款用 Rust 和 egui 开发的桌面端日志查看与过滤工具，支持 Android（通过 adb 的 logcat）与 HarmonyOS（通过 hdc 的 hilog），专注于打开大型日志文件、流式显示设备输出，以及按级别、进程、线程、标签和消息文本快速筛选日志。

## 功能特性

- 打开本地日志文件，支持拖放和最近文件历史。
- 大型 UTF-8 日志根据预计内存自动切换到稀疏磁盘索引和按 RAM 自适应的分页浏览，
  无需把完整文件载入内存。
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

每次运行或重启采集都会在用户数据目录下的 `logs/` 中创建新的时间戳 `.log` 文件
（例如 Linux 的 `~/.local/share/logfilter/logs`、Windows 的
`%LOCALAPPDATA%\logfilter\logs`）。
这是持久文件，不是临时文件；清空预览和退出程序都不会删除它。GUI 同时显示
`logs` 目录和当前文件的完整路径。

原始 stdout 字节先写入磁盘，预览再从文件读取。过滤和绘制不会阻塞录制线程。
暂停时仍会排空设备管道，但暂停期间收到的日志不会写入文件，也不会额外提示。
Clear 从当前录制位置重新开始预览，完整录制文件保持不变。缓存到达
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

默认至少保留 1 GiB 磁盘空间；达到该条件会停止采集并报错，已有文件保持不变。
LogFilter 不会静默删除或覆盖历史录制文件。

也可以在配置文件中设置：

```toml
[recording]
cache_mib = 0 # 自动；也可设置为 2048 等数值
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

### 大文件分页浏览

小于 256 MiB 的文件正常完整加载。256 MiB～4 GiB 的文件会抽样前 8 MiB，
根据原始字节、预计行数、每行结构／索引开销和 25% 安全余量估算完整解析内存；
预计成本超过日志内存预算的 60% 时进入分页。达到 4 GiB 的文件强制分页。

分页模式在后台扫描文件，为每页保存一个字节偏移检查点。解析行的内存预算为
日志内存预算的一半，限制在 32 MiB～8 GiB。抽样会选出 20,000～5,000,000 行的
每页行数上限；每页同时受字节预算约束，计入无效 UTF-8 的替换膨胀及行和索引开销。
长行会让页面容纳的行数减少，后续行仍可连续翻页查看。索引内存随页数增长，
无需为每行保存偏移。页面逐步可用，跳转通过检查点定位原始行号。
切换页面或过滤条件会取消旧读取，同一文件每次只进行一个页面读取，
读取期间保留当前显示页。

查找、排除、级别、PID、TID 和标签过滤会在后台扫描完整文件，并建立独立的稀疏
匹配索引。过滤后的页面保留原始文件行号。没有启用全局过滤时，跳转使用原始行号。
过滤导出会按当前过滤条件快照流式扫描完整文件。书签按原始行号保留，翻页不会丢失；
“仅书签”导出包含当前文件所有页的书签，并同时应用其他过滤条件。重新打开或清空文件会清除书签。
列选择器计数和书签标记只描述当前页；选择某个列值后，该值仍会作为全文件过滤条件。

分页模式目前仅支持 UTF-8。文件按快照处理；索引后文件发生变化时需要重新打开。
翻页和导出均会在读取前后校验打开时的快照；导出失败会移除不完整的输出文件。
源文件单行超过 64 MiB 或页面内存预算时会明确报错。预算约束的是解析数据的估算
内存，界面渲染、索引和有界读写缓冲区还会占用额外内存。

## adb 流式日志

LogFilter 可以从工具栏直接启动 adb 命令。默认包含以下预设命令：

- `logcat -v threadtime`
- `logcat -v time`
- `logcat -b radio -v time`
- `logcat -b events -v time`
- `shell cat /proc/kmsg`

如果 `adb` 不在 `PATH` 环境变量中，可以在配置文件中设置 `adb.adb_path`。在 Windows 上，应用还会检查默认的 Android Studio SDK 路径。

## HarmonyOS 日志兼容

内置 hdc 日志预设通过设备 shell 检查工具：有 `hilog` 时使用原预设，没有时回退到
`hilogcat` 的默认输出。两者参数并不通用，因此回退不会沿用 `-v` 或 `-D`；
域预设无法应用时会提示，可改用界面的标签过滤。两个工具都不存在时明确报错。
自定义命令不改写。两个域预设按子系统过滤：`0x2F00`（Security，系统安全）与
`0x2D00`（DFX，调测框架），名称取自 OpenHarmony hilog 官方域表。

预设使用纯 `hilog`（或 `hilog -v time`）流式读取，刻意不带 `-r`：按官方 hilog
源码，`-r` 会清空设备日志缓冲区并退出，等于把正在采集的日志删掉。旧版本曾
附带带 `-r`（以及无效 `threadtime` 格式）的预设；修复前保存的配置在加载时会
自动迁移到安全等价命令，旧字符串若被手动输入也会解析到安全命令。

hdc 遇到协议错误（目标不匹配、设备掉线）时，会在 **stdout** 打印 `[Fail] …`
并以 0 退出；LogFilter 会识别该情况：不写入任何数据，并报告失败，而不是当成
普通的“会话结束”。

该回退尚需对应 HarmonyOS NEXT 真机验证。

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

Apache-2.0，详见 [LICENSE](LICENSE)。
