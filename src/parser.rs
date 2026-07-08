use crate::model::{LevelMask, LogEntry, LogFormat};
use regex::Regex;
use std::sync::OnceLock;

fn re_threadtime() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^(?P<date>\d{2}-\d{2})\s+(?P<time>\d{2}:\d{2}:\d{2}\.\d{3})\s+(?P<pid>\d+)\s+(?P<tid>\d+)\s+(?P<lv>[VDIWEFA])\s+(?P<tag>[^:]+?):\s?(?P<msg>.*)$",
        )
        .unwrap()
    })
}

fn re_time() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^(?P<date>\d{2}-\d{2})\s+(?P<time>\d{2}:\d{2}:\d{2}\.\d{3})\s+(?P<lv>[VDIWEFA])/(?P<tag>[^()]+?)\(\s*(?P<pid>\d+)\s*\):\s?(?P<msg>.*)$",
        )
        .unwrap()
    })
}

fn re_brief() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^(?P<lv>[VDIWEFA])/(?P<tag>[^()]+?)\(\s*(?P<pid>\d+)\s*\):\s?(?P<msg>.*)$",
        )
        .unwrap()
    })
}

fn re_kernel() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^<(?P<lv>[0-7])>\[?\s*(?P<time>[\d.]+)\]?\s?(?P<msg>.*)$").unwrap()
    })
}

pub fn parse_line(line: &str) -> (LogEntry, LogFormat) {
    if let Some(c) = re_threadtime().captures(line) {
        return (
            LogEntry {
                line_no: 0,
                date: c["date"].to_string(),
                time: c["time"].to_string(),
                pid: c["pid"].to_string(),
                tid: c["tid"].to_string(),
                level: LevelMask::from_char(c["lv"].chars().next().unwrap()).unwrap_or(LevelMask::V),
                tag: c["tag"].trim().to_string(),
                message: c["msg"].to_string(),
            },
            LogFormat::ThreadTime,
        );
    }
    if let Some(c) = re_time().captures(line) {
        return (
            LogEntry {
                line_no: 0,
                date: c["date"].to_string(),
                time: c["time"].to_string(),
                pid: c["pid"].to_string(),
                tid: String::new(),
                level: LevelMask::from_char(c["lv"].chars().next().unwrap()).unwrap_or(LevelMask::V),
                tag: c["tag"].trim().to_string(),
                message: c["msg"].to_string(),
            },
            LogFormat::Time,
        );
    }
    if let Some(c) = re_brief().captures(line) {
        return (
            LogEntry {
                line_no: 0,
                date: String::new(),
                time: String::new(),
                pid: c["pid"].to_string(),
                tid: String::new(),
                level: LevelMask::from_char(c["lv"].chars().next().unwrap()).unwrap_or(LevelMask::V),
                tag: c["tag"].trim().to_string(),
                message: c["msg"].to_string(),
            },
            LogFormat::Brief,
        );
    }
    if let Some(c) = re_kernel().captures(line) {
        let digit: u8 = c["lv"].parse().unwrap_or(7);
        return (
            LogEntry {
                line_no: 0,
                date: String::new(),
                time: c["time"].to_string(),
                pid: String::new(),
                tid: String::new(),
                level: LevelMask::from_kernel_digit(digit),
                tag: String::new(),
                message: c["msg"].to_string(),
            },
            LogFormat::Kernel,
        );
    }
    (
        LogEntry {
            line_no: 0,
            date: String::new(),
            time: String::new(),
            pid: String::new(),
            tid: String::new(),
            level: LevelMask::V,
            tag: String::new(),
            message: line.to_string(),
        },
        LogFormat::Unknown,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_threadtime() {
        let line = "01-01 12:34:56.789  1234  5678 I ActivityManager: Start proc";
        let (e, f) = parse_line(line);
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.date, "01-01");
        assert_eq!(e.time, "12:34:56.789");
        assert_eq!(e.pid, "1234");
        assert_eq!(e.tid, "5678");
        assert_eq!(e.level, LevelMask::I);
        assert_eq!(e.tag, "ActivityManager");
        assert_eq!(e.message, "Start proc");
    }

    #[test]
    fn parses_time() {
        let line = "01-01 12:34:56.789 D/MyTag( 1234): a message";
        let (e, f) = parse_line(line);
        assert_eq!(f, LogFormat::Time);
        assert_eq!(e.level, LevelMask::D);
        assert_eq!(e.tag, "MyTag");
        assert_eq!(e.pid, "1234");
        assert_eq!(e.message, "a message");
    }

    #[test]
    fn parses_brief() {
        let line = "W/Tag(  42): hi";
        let (e, f) = parse_line(line);
        assert_eq!(f, LogFormat::Brief);
        assert_eq!(e.level, LevelMask::W);
        assert_eq!(e.tag, "Tag");
        assert_eq!(e.pid, "42");
    }

    #[test]
    fn parses_kernel() {
        let line = "<3>[  12.345] usb: disconnect";
        let (e, f) = parse_line(line);
        assert_eq!(f, LogFormat::Kernel);
        assert_eq!(e.level, LevelMask::E);
        assert_eq!(e.message, "usb: disconnect");
    }

    #[test]
    fn fallback_unknown() {
        let line = "totally random garbage";
        let (e, f) = parse_line(line);
        assert_eq!(f, LogFormat::Unknown);
        assert_eq!(e.message, "totally random garbage");
    }
}
