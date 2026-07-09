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

type Span = (u32, u32);
const EMPTY: Span = (0, 0);

/// Byte range of a named capture within the line (`(0,0)` if absent).
fn span(c: &regex::Captures, name: &str) -> Span {
    match c.name(name) {
        Some(m) => (m.start() as u32, m.end() as u32),
        None => EMPTY,
    }
}

/// Trim ASCII whitespace off both ends of a span, returning the tighter range.
fn trim_span(line: &str, sp: Span) -> Span {
    let sub = &line[sp.0 as usize..sp.1 as usize];
    let trimmed = sub.trim();
    if trimmed.is_empty() {
        return (sp.0, sp.0);
    }
    // `trimmed` is a subslice of `sub`; recover its offset by pointer delta.
    let start = sp.0 + (trimmed.as_ptr() as usize - sub.as_ptr() as usize) as u32;
    (start, start + trimmed.len() as u32)
}

pub fn parse_line(line: String) -> (LogEntry, LogFormat) {
    // Extract spans + level in an inner scope so the regex captures (which
    // borrow `line`) are dropped before we move `line` into the entry's buffer.
    let parsed: Option<(LogFormat, LevelMask, [Span; 6])> = {
        let s = line.as_str();
        if let Some(c) = re_threadtime().captures(s) {
            let lv = LevelMask::from_char(c["lv"].chars().next().unwrap()).unwrap_or(LevelMask::V);
            Some((LogFormat::ThreadTime, lv, [
                span(&c, "date"), span(&c, "time"), span(&c, "pid"),
                span(&c, "tid"), trim_span(s, span(&c, "tag")), span(&c, "msg"),
            ]))
        } else if let Some(c) = re_time().captures(s) {
            let lv = LevelMask::from_char(c["lv"].chars().next().unwrap()).unwrap_or(LevelMask::V);
            Some((LogFormat::Time, lv, [
                span(&c, "date"), span(&c, "time"), span(&c, "pid"),
                EMPTY, trim_span(s, span(&c, "tag")), span(&c, "msg"),
            ]))
        } else if let Some(c) = re_brief().captures(s) {
            let lv = LevelMask::from_char(c["lv"].chars().next().unwrap()).unwrap_or(LevelMask::V);
            Some((LogFormat::Brief, lv, [
                EMPTY, EMPTY, span(&c, "pid"),
                EMPTY, trim_span(s, span(&c, "tag")), span(&c, "msg"),
            ]))
        } else if let Some(c) = re_kernel().captures(s) {
            let digit: u8 = c["lv"].parse().unwrap_or(7);
            Some((LogFormat::Kernel, LevelMask::from_kernel_digit(digit), [
                EMPTY, span(&c, "time"), EMPTY, EMPTY, EMPTY, span(&c, "msg"),
            ]))
        } else {
            None
        }
    };

    match parsed {
        Some((fmt, level, [date, time, pid, tid, tag, msg])) => (
            LogEntry::new(line.into_boxed_str(), level, date, time, pid, tid, tag, msg),
            fmt,
        ),
        None => {
            // Unknown format: the whole line is the message.
            let end = line.len() as u32;
            (
                LogEntry::new(line.into_boxed_str(), LevelMask::V, EMPTY, EMPTY, EMPTY, EMPTY, EMPTY, (0, end)),
                LogFormat::Unknown,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_threadtime() {
        let line = "01-01 12:34:56.789  1234  5678 I ActivityManager: Start proc";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.date(), "01-01");
        assert_eq!(e.time(), "12:34:56.789");
        assert_eq!(e.pid(), "1234");
        assert_eq!(e.tid(), "5678");
        assert_eq!(e.level, LevelMask::I);
        assert_eq!(e.tag(), "ActivityManager");
        assert_eq!(e.message(), "Start proc");
    }

    #[test]
    fn parses_time() {
        let line = "01-01 12:34:56.789 D/MyTag( 1234): a message";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::Time);
        assert_eq!(e.level, LevelMask::D);
        assert_eq!(e.tag(), "MyTag");
        assert_eq!(e.pid(), "1234");
        assert_eq!(e.message(), "a message");
    }

    #[test]
    fn parses_brief() {
        let line = "W/Tag(  42): hi";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::Brief);
        assert_eq!(e.level, LevelMask::W);
        assert_eq!(e.tag(), "Tag");
        assert_eq!(e.pid(), "42");
    }

    #[test]
    fn parses_kernel() {
        let line = "<3>[  12.345] usb: disconnect";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::Kernel);
        assert_eq!(e.level, LevelMask::E);
        assert_eq!(e.message(), "usb: disconnect");
    }

    #[test]
    fn fallback_unknown() {
        let line = "totally random garbage";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::Unknown);
        assert_eq!(e.message(), "totally random garbage");
    }
}
