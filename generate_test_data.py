#!/usr/bin/env python3
"""Generate test logcat files for LogFilter testing."""
import os, random, datetime

TEST_DIR = os.path.expanduser("~/.config/logfilter/test_data")
os.makedirs(TEST_DIR, exist_ok=True)

# ── ThreadTime format (logcat -v threadtime) ──────────────
TAGS = ["ActivityManager", "SystemServer", "Binder:1234_A", "WindowManager",
        "InputDispatcher", "PowerManager", "NetworkStack", "WifiManager",
        "BluetoothManager", "CameraService", "MediaCodec", "SurfaceFlinger",
        "MyApp", "com.example.app", "dalvikvm", "art", "BufferQueue",
        "AudioFlinger", "SensorService", "UsbService"]
PIDS = [1234, 5678, 9012, 3456, 7890, 1111, 2222, 3333, 4444, 5555]
TIDS = [1234, 5678, 9012, 3456, 7890, 1111, 2222, 3333, 4444, 5555,
        6666, 7777, 8888, 9999, 1010, 2020, 3030, 4040, 5050, 6060]
LEVELS = ["V", "D", "I", "W", "E", "F"]

MSG_TEMPLATES = [
    "Starting service: Intent act=android.intent.action...",
    "Scheduling alarm: id={} interval={}ms",
    "Received broadcast: android.intent.action.BATTERY_CHANGED level={} scale=100",
    "dispatchTouchEvent: action=0, x={}, y={}",
    "handleMessage: what={} arg1={}",
    "connectToNetwork: network={} capabilities=[INTERNET, NOT_METERED]",
    "onReceive: action=android.net.wifi.SCAN_RESULTS count={}",
    "Camera {}: exposure compensation range [-6,6]",
    "writeToParcel: size={} bytes",
    "readFromParcel: size={} bytes",
    "updateConfiguration: config={}",
    "handleSyncOperation: authority={}",
    "ActivityRecord uuid={} resumed",
    "Process {}: scheduling for restart in {}ms",
    "surfaceChanged: format={} width={} height={}",
    "enqueueBuffer: buffer={}, pending={}",
    "requestBuffer: buffer={}, status=OK",
    "notifyMemoryPressure: level={}",
    "stopMonitoring: id={}",
    "thermal: cpu_temp={}C battery_temp={}C",
]

# Target patterns for find/remove/highlight testing
PATTERN_TAGS = ["NetworkStack", "WifiManager", "BluetoothManager"]
PATTERN_MSGS = ["error", "warn", "fail", "exception", "crash", "timeout",
                "success", "complete", "loaded", "connected", "disconnected",
                "battery", "thermal", "memory", "cpu_temp"]

def threadtime_line(date, time, pid, tid, lv, tag, msg):
    return f"{date} {time:>5s} {pid:>5d} {tid:>5d} {lv} {tag}: {msg}"

def brief_line(lv, tag, pid, msg):
    return f"{lv}/{tag}({pid:>5d}): {msg}"

def time_line(date, time, lv, tag, pid, msg):
    return f"{date} {time:>5s} {lv}/{tag}({pid:>5d}): {msg}"

def kernel_line(msg):
    lv = random.randint(0, 7)
    ts = random.uniform(0, 999.999)
    return f"<{lv}>[{ts:>8.3f}] {msg}"

# ── Generate a large mixed-format log file ──
lines = []
date = "07-10"  # Fixed date for our testing
base_hour = 10
base_min = 0
base_sec = 0.0

# Start with 1000 lines of ThreadTime format
for i in range(1000):
    pid = random.choice(PIDS)
    tid = random.choice(TIDS)
    lv = random.choice(LEVELS)
    tag = random.choice(TAGS)
    msg = random.choice(MSG_TEMPLATES).format(
        *[random.randint(0, 1000) for _ in range(20)]
    )
    # Inject error/warn patterns in some tags
    if tag in PATTERN_TAGS and random.random() < 0.3:
        msg = f"error: {msg}"[:120]
        lv = "E"
    # Timestamp progression
    sec = i * 0.487 + base_sec
    m = int(sec // 60)
    s = sec % 60
    time = f"{base_hour + m:02d}:{base_min:02d}:{s:06.3f}".replace(".", ".")
    line = threadtime_line(date, time, pid, tid, lv, tag, msg)
    lines.append((i, line))

# Then add 500 lines of Brief format
for i in range(500):
    pid = random.choice(PIDS)
    lv = random.choice(LEVELS)
    tag = random.choice(TAGS)
    msg = random.choice(MSG_TEMPLATES).format(
        *[random.randint(0, 1000) for _ in range(20)]
    )
    if tag == "MyApp" and random.random() < 0.5:
        msg = f"connected to server at 10.0.{random.randint(0,255)}.{random.randint(0,255)}"
    line = brief_line(lv, tag, pid, msg)
    lines.append((i + 1000, line))

# Then add 300 lines of Time format
for i in range(300):
    pid = random.choice(PIDS)
    lv = random.choice(LEVELS)
    tag = random.choice(TAGS)
    msg = random.choice(MSG_TEMPLATES).format(
        *[random.randint(0, 1000) for _ in range(20)]
    )
    sec = i * 1.2 + base_sec
    m = int(sec // 60)
    s = sec % 60
    time = f"{base_hour + 2:02d}:{base_min:02d}:{s:05.2f}"
    line = time_line(date, time, lv, tag, pid, msg)
    lines.append((i + 1500, line))

# Then add 200 lines of Kernel format
for i in range(200):
    msg = f"kernel: usb {random.randint(1,4)}-{random.randint(1,4)}: new device"
    if random.random() < 0.2:
        msg = f"kernel: OOM killer invoked for pid={random.choice(PIDS)}"
    line = kernel_line(msg)
    lines.append((i + 1800, line))

lines.sort(key=lambda x: x[0])
with open(os.path.join(TEST_DIR, "mixed_logcat.txt"), "w") as f:
    for _, line in lines:
        f.write(line + "\n")
print(f"Generated {len(lines)}-line mixed-format file")

# ── Generate a huge file for stress testing ──
lines2 = []
TAGS_BIG = ["System", "App1", "App2", "Network", "Camera", "Audio", "Input", "Power"]
for i in range(100000):
    pid = random.choice(PIDS)
    tid = random.choice(TIDS)
    lv = random.choice(LEVELS)
    tag = random.choice(TAGS_BIG)
    msg = f"Message text line {i}: pid={pid} tid={tid} level={lv} tag={tag} " + \
          "A" * random.randint(10, 200)
    # Insert specific patterns
    if i % 100 == 0:
        msg = f"ERROR: {msg}"
        lv = "E"
    elif i % 150 == 0:
        msg = f"WARNING: {msg}"
        lv = "W"
    elif i % 200 == 0:
        msg = f"TAGET_STRING_{i}: {lv} / {tag} / {pid}"
    sec = i * 0.051
    m = int(sec // 60)
    s = sec % 60
    time = f"{base_hour + m:02d}:00:{s:06.3f}"
    line = threadtime_line(date, time, pid, tid, lv, tag, msg)
    lines2.append(line)

with open(os.path.join(TEST_DIR, "large_logcat_100k.txt"), "w") as f:
    for line in lines2:
        f.write(line + "\n")
print(f"Generated {len(lines2)}-line stress-test file")

# ── Generate a UTF-16 encoded file ──
utf16_lines = lines[:50]
with open(os.path.join(TEST_DIR, "utf16_logcat.txt"), "w", encoding="utf-16-le") as f:
    for _, line in utf16_lines:
        f.write(line + "\n")
print(f"Generated {len(utf16_lines)}-line UTF-16 file")

# ── Generate a UTF-8 BOM file ──
with open(os.path.join(TEST_DIR, "utf8_bom_logcat.txt"), "wb") as f:
    f.write(b"\xef\xbb\xbf")
    for _, line in lines[:50]:
        f.write((line + "\n").encode("utf-8"))
print(f"Generated 50-line UTF-8 BOM file")

# ── Single-format test files ──
# ThreadTime only
with open(os.path.join(TEST_DIR, "threadtime_logcat.txt"), "w") as f:
    for _, line in lines[:100]:
        f.write(line + "\n")
print("Generated 100-line threadtime file")

# Target pattern file for find/remove/highlight testing
pattern_lines = []
PATTERN_SRCS = ["error", "exception", "failed", "timeout", "crash",
                "success", "connected", "loaded", "complete", "started",
                "battery", "memory", "cpu", "network", "wifi", "bluetooth"]
for i in range(500):
    pid = random.choice(PIDS)
    tid = random.choice(TIDS)
    src = random.choice(PATTERN_SRCS)
    lv = "E" if src in ("error", "exception", "failed", "crash", "timeout") else random.choice(LEVELS)
    tag = random.choice(TAGS)
    msg = f"[{src.upper()}] log_{i}: {random.choice(MSG_TEMPLATES).format(*[random.randint(0,1000) for _ in range(20)])}"
    sec = i * 0.3
    m = int(sec // 60)
    s = sec % 60
    time = f"{base_hour:02d}:{base_min:02d}:{s:06.3f}"
    line = threadtime_line(date, time, pid, tid, lv, tag, msg)
    pattern_lines.append(line)

with open(os.path.join(TEST_DIR, "patterns_logcat.txt"), "w") as f:
    for line in pattern_lines:
        f.write(line + "\n")
print(f"Generated {len(pattern_lines)}-line pattern file")

# Chinese characters test
cn_lines = []
cn_tags = ["系统服务", "网络管理", "应用商店", "相机", "蓝牙", "WiFi", "系统更新"]
cn_msgs = [
    "启动服务: 已完成初始化",
    "网络连接成功: IP 地址 192.168.1.100",
    "系统更新检查: 发现新版本 v3.2.1",
    "内存使用: 已用 1.2GB / 总计 4GB",
    "电池电量: 85%, 温度 37.5°C",
    "蓝牙设备已配对: My Headphones",
    "相机启动: 分辨率 1920x1080",
    "文件下载完成: update_package_v3.2.1.zip",
    "用户登录: user_id=10086",
    "数据库查询: SELECT * FROM logs WHERE level='ERROR'",
]
for i in range(100):
    pid = random.choice(PIDS[:3])
    tid = random.choice(TIDS[:3])
    lv = random.choice(LEVELS)
    tag = random.choice(cn_tags)
    msg = random.choice(cn_msgs)
    sec = i * 1.5
    m = int(sec // 60)
    s = sec % 60
    time = f"{base_hour:02d}:{base_min:02d}:{s:06.3f}"
    line = threadtime_line(date, time, pid, tid, lv, tag, msg)
    cn_lines.append(line)

with open(os.path.join(TEST_DIR, "chinese_logcat.txt"), "w") as f:
    for line in cn_lines:
        f.write(line + "\n")
print(f"Generated {len(cn_lines)}-line Chinese-char file")

print(f"\nAll test files in: {TEST_DIR}")
print("Files:")
for f in sorted(os.listdir(TEST_DIR)):
    sz = os.path.getsize(os.path.join(TEST_DIR, f))
    print(f"  {f}: {sz:,} bytes")
