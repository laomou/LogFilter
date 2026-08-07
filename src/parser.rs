use crate::model::{LevelMask, LogEntry, LogFormat};
use regex::Regex;
use std::sync::OnceLock;

fn re_threadtime() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^(?P<date>\d{2}-\d{2})\s+(?P<time>\d{2}:\d{2}:\d{2}\.\d{3})\s+(?P<pid>\d+)\s+(?P<tid>\d+)\s+(?:(?P<uid>\d+)\s+)?(?P<lv>[VDIWEFA])\s+(?P<tag>[^:]+?):\s?(?P<msg>.*)$",
        )
        .unwrap()
    })
}

/// HarmonyOS `hilog`: `MM-DD HH:MM:SS.sss PID TID Level Domain/Tag: message`.
/// Differs from threadtime by the `Domain/Tag` field and a fractional-second
/// part that may be 3–9 digits (ms/µs/ns). The domain is a log-type letter
/// (`A`=app, `C`=core, `I`=init/system, …) followed by hex digits, e.g.
/// `A06666`, `C01719`, `I00000`. Requiring an uppercase letter + ≥4 hex digits
/// means this doesn't claim an Android tag that merely starts with a short hex
/// word then a slash (e.g. `DB/query`); any that still slip through fall back to
/// lossless ThreadTime. Safe to try before threadtime.
fn re_hilog() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^(?P<date>\d{2}-\d{2})\s+(?P<time>\d{2}:\d{2}:\d{2}\.\d{3,9})\s+(?P<pid>\d+)\s+(?P<tid>\d+)\s+(?P<lv>[VDIWEFA])\s+(?P<domain>[A-Z][0-9A-Fa-f]{4,})/(?P<tag>[^:]+?):\s?(?P<msg>.*)$",
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
        Regex::new(r"^(?P<lv>[VDIWEFA])/(?P<tag>[^()]+?)\(\s*(?P<pid>\d+)\s*\):\s?(?P<msg>.*)$")
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

#[allow(dead_code)]
pub fn parse_line(line: String) -> (LogEntry, LogFormat) {
    parse_line_hinted(line, LogFormat::Unknown)
}

/// Like [`parse_line`] but tries `hint` first to avoid redundant regex attempts
/// on homogeneous log streams. Falls back to the full scan if the hint misses.
pub fn parse_line_hinted(line: String, hint: LogFormat) -> (LogEntry, LogFormat) {
    // Extract spans + level in an inner scope so the regex captures (which
    // borrow `line`) are dropped before we move `line` into the entry's buffer.
    let parsed: Option<(LogFormat, LevelMask, [Span; 7])> = {
        let s = line.as_str();

        // Helper closures — called at most once each.
        let try_threadtime = |s: &str| {
            re_threadtime().captures(s).map(|c| {
                let lv = c["lv"]
                    .chars()
                    .next()
                    .and_then(LevelMask::from_char)
                    .unwrap_or(LevelMask::V);
                (
                    LogFormat::ThreadTime,
                    lv,
                    [
                        span(&c, "date"),
                        span(&c, "time"),
                        span(&c, "pid"),
                        span(&c, "tid"),
                        span(&c, "uid"),
                        trim_span(s, span(&c, "tag")),
                        span(&c, "msg"),
                    ],
                )
            })
        };
        let try_hilog = |s: &str| {
            re_hilog().captures(s).map(|c| {
                let lv = c["lv"]
                    .chars()
                    .next()
                    .and_then(LevelMask::from_char)
                    .unwrap_or(LevelMask::V);
                (
                    LogFormat::HiLog,
                    lv,
                    [
                        span(&c, "date"),
                        span(&c, "time"),
                        span(&c, "pid"),
                        span(&c, "tid"),
                        EMPTY,
                        // Tag column shows the full `domain/tag` (e.g.
                        // `I02C01/MyTag`) — the HarmonyOS domain identifies the
                        // subsystem, so keep it visible rather than dropping it.
                        trim_span(s, (span(&c, "domain").0, span(&c, "tag").1)),
                        span(&c, "msg"),
                    ],
                )
            })
        };
        let try_time = |s: &str| {
            re_time().captures(s).map(|c| {
                let lv = c["lv"]
                    .chars()
                    .next()
                    .and_then(LevelMask::from_char)
                    .unwrap_or(LevelMask::V);
                (
                    LogFormat::Time,
                    lv,
                    [
                        span(&c, "date"),
                        span(&c, "time"),
                        span(&c, "pid"),
                        EMPTY,
                        EMPTY,
                        trim_span(s, span(&c, "tag")),
                        span(&c, "msg"),
                    ],
                )
            })
        };
        let try_brief = |s: &str| {
            re_brief().captures(s).map(|c| {
                let lv = c["lv"]
                    .chars()
                    .next()
                    .and_then(LevelMask::from_char)
                    .unwrap_or(LevelMask::V);
                (
                    LogFormat::Brief,
                    lv,
                    [
                        EMPTY,
                        EMPTY,
                        span(&c, "pid"),
                        EMPTY,
                        EMPTY,
                        trim_span(s, span(&c, "tag")),
                        span(&c, "msg"),
                    ],
                )
            })
        };
        let try_kernel = |s: &str| {
            re_kernel().captures(s).map(|c| {
                let digit: u8 = c["lv"].parse().unwrap_or(7);
                (
                    LogFormat::Kernel,
                    LevelMask::from_kernel_digit(digit),
                    [
                        EMPTY,
                        span(&c, "time"),
                        EMPTY,
                        EMPTY,
                        EMPTY,
                        EMPTY,
                        span(&c, "msg"),
                    ],
                )
            })
        };

        // Try the hinted format first; fall back to the full scan only on a miss.
        let hinted = match hint {
            LogFormat::ThreadTime => try_threadtime(s),
            LogFormat::HiLog => try_hilog(s),
            LogFormat::Time => try_time(s),
            LogFormat::Brief => try_brief(s),
            LogFormat::Kernel => try_kernel(s),
            LogFormat::Unknown => None,
        };

        if hinted.is_some() {
            hinted
        } else {
            // Full scan in priority order (HiLog → ThreadTime → Time → Brief →
            // Kernel), skipping whichever format the hint already tried. HiLog is
            // tried before ThreadTime because a hilog line would otherwise match
            // threadtime with the domain glued onto the tag.
            match hint {
                LogFormat::HiLog => try_threadtime(s)
                    .or_else(|| try_time(s))
                    .or_else(|| try_brief(s))
                    .or_else(|| try_kernel(s)),
                LogFormat::ThreadTime => try_hilog(s)
                    .or_else(|| try_time(s))
                    .or_else(|| try_brief(s))
                    .or_else(|| try_kernel(s)),
                LogFormat::Time => try_hilog(s)
                    .or_else(|| try_threadtime(s))
                    .or_else(|| try_brief(s))
                    .or_else(|| try_kernel(s)),
                LogFormat::Brief => try_hilog(s)
                    .or_else(|| try_threadtime(s))
                    .or_else(|| try_time(s))
                    .or_else(|| try_kernel(s)),
                LogFormat::Kernel => try_hilog(s)
                    .or_else(|| try_threadtime(s))
                    .or_else(|| try_time(s))
                    .or_else(|| try_brief(s)),
                LogFormat::Unknown => try_hilog(s)
                    .or_else(|| try_threadtime(s))
                    .or_else(|| try_time(s))
                    .or_else(|| try_brief(s))
                    .or_else(|| try_kernel(s)),
            }
        }
    };

    match parsed {
        Some((fmt, level, [date, time, pid, tid, uid, tag, msg])) => (
            LogEntry::new(
                line.into_boxed_str(),
                level,
                date,
                time,
                pid,
                tid,
                uid,
                tag,
                msg,
            ),
            fmt,
        ),
        None => {
            // Unknown format: the whole line is the message.
            let end = line.len() as u32;
            (
                LogEntry::new(
                    line.into_boxed_str(),
                    LevelMask::V,
                    EMPTY,
                    EMPTY,
                    EMPTY,
                    EMPTY,
                    EMPTY,
                    EMPTY,
                    (0, end),
                ),
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
    fn parses_hilog_harmonyos() {
        // HarmonyOS hilog: Domain/Tag field; tag column shows just the tag.
        let line = "08-06 15:43:53.405  1234  5678 I A03D00/MyApp: key frame id: 0";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.date(), "08-06");
        assert_eq!(e.time(), "15:43:53.405");
        assert_eq!(e.pid(), "1234");
        assert_eq!(e.tid(), "5678");
        assert_eq!(e.level, LevelMask::I);
        assert_eq!(e.tag(), "A03D00/MyApp", "full domain/tag shown");
        assert_eq!(e.message(), "key frame id: 0");
    }

    #[test]
    fn parses_hilog_microsecond_timestamp() {
        // hilog timestamps can carry more than 3 fractional-second digits.
        let line = "08-06 15:43:53.405123  900  901 W C01719/HiSysEvent: warn body";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.time(), "15:43:53.405123");
        assert_eq!(e.level, LevelMask::W);
        assert_eq!(e.tag(), "C01719/HiSysEvent");
        assert_eq!(e.message(), "warn body");
    }

    #[test]
    fn parses_real_harmonyos_hilog_output() {
        // Verbatim lines from `hdc hilog` on a real HarmonyOS device (all -v
        // variants emit pid+tid and a `A#####/tag` field). The tag can be a
        // multi-segment path and the message can contain `:` and `[...]`.
        let (e, f) = parse_line(
            "08-07 12:02:47.453 23285 23285 W A06666/AppGalleryService/AG/AppGallery: \
             [AppRunModeImpl]:isAgreedFromSettings cache, return from:ServiceInstance"
                .to_string(),
        );
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.date(), "08-07");
        assert_eq!(e.time(), "12:02:47.453");
        assert_eq!(e.pid(), "23285");
        assert_eq!(e.tid(), "23285");
        assert_eq!(e.level, LevelMask::W);
        assert_eq!(e.tag(), "A06666/AppGalleryService/AG/AppGallery");
        assert_eq!(
            e.message(),
            "[AppRunModeImpl]:isAgreedFromSettings cache, return from:ServiceInstance"
        );

        // Package-name-style tag.
        let (e, f) = parse_line(
            "08-07 12:02:47.772 23643 23643 I \
             A02F08/com.huawei.hmos.brid/Security_BRID_Service: local RiskFactor is valid."
                .to_string(),
        );
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.tag(), "A02F08/com.huawei.hmos.brid/Security_BRID_Service");
        assert_eq!(e.message(), "local RiskFactor is valid.");

        // Message leads with bracketed hex trace ids — must not confuse tag/domain.
        let (e, f) = parse_line(
            "08-07 12:07:58.900 32181 32181 I A0A5A5/com.huawei.hmos.hiviewcare/Diagnosis: \
             [a92ab15a1ecb690 4ad1ee 3d06e79][HCWatchCloudRegister]allWatchInfo size is 0"
                .to_string(),
        );
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.tag(), "A0A5A5/com.huawei.hmos.hiviewcare/Diagnosis");
        assert_eq!(
            e.message(),
            "[a92ab15a1ecb690 4ad1ee 3d06e79][HCWatchCloudRegister]allWatchInfo size is 0"
        );

        // Init/system logs use a non-hex type-letter domain (`I…`), and pid/tid
        // can be 0 during early boot. Regression: the old hex-only domain pattern
        // rejected these, so they fell through to ThreadTime with the domain glued
        // into the tag.
        let (e, f) = parse_line(
            "08-07 13:33:33.641     0     0 I I00000/HiLog: ========Zeroth log of type: init"
                .to_string(),
        );
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.pid(), "0");
        assert_eq!(e.tag(), "I00000/HiLog", "type-letter domain kept in tag");
        assert_eq!(e.message(), "========Zeroth log of type: init");

        let (e, f) = parse_line(
            "08-07 13:33:33.556   532   532 I \
             I02C01/hmos_cust_carrier_mount/CustCarrierMount: MountCarrierToShared start"
                .to_string(),
        );
        assert_eq!(f, LogFormat::HiLog);
        assert_eq!(e.tag(), "I02C01/hmos_cust_carrier_mount/CustCarrierMount");
        assert_eq!(e.message(), "MountCarrierToShared start");
    }

    #[test]
    fn android_threadtime_not_misparsed_as_hilog() {
        // A plain Android tag has no "hexdomain/" prefix, so HiLog must not claim
        // it — it stays ThreadTime with the whole tag intact.
        let line = "01-01 12:34:56.789  1234  5678 I ActivityManager: Start proc";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.tag(), "ActivityManager");
    }

    #[test]
    fn short_hex_slash_tag_stays_threadtime() {
        // An Android tag that starts with a short hex word then a slash must NOT
        // be claimed by HiLog (its domain requires ≥5 hex digits) — the whole
        // "DB/query" tag stays intact under ThreadTime.
        let line = "01-01 12:34:56.789  1234  5678 I DB/query: run";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.tag(), "DB/query");
        assert_eq!(e.message(), "run");
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

    #[test]
    fn hinted_threadtime_hits_fast_path() {
        let line = "01-01 12:34:56.789  1 2 I Tag: msg";
        let (e, f) = parse_line_hinted(line.to_string(), LogFormat::ThreadTime);
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.tag(), "Tag");
        assert_eq!(e.message(), "msg");
    }

    #[test]
    fn hint_mismatch_falls_back() {
        // Hint says Brief, but it's actually ThreadTime — should still parse correctly.
        let line = "01-01 12:34:56.789  100  200 W SysUI: drawn";
        let (e, f) = parse_line_hinted(line.to_string(), LogFormat::Brief);
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.level, LevelMask::W);
        assert_eq!(e.tag(), "SysUI");
    }

    #[test]
    fn multibyte_tag() {
        let line = "01-01 12:34:56.789  1 2 I 日志Tag: 消息body";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.tag(), "日志Tag");
        assert_eq!(e.message(), "消息body");
    }

    #[test]
    fn empty_message_after_colon() {
        let line = "01-01 12:34:56.789  1 2 I Tag:";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.tag(), "Tag");
        assert_eq!(e.message(), "");
    }

    #[test]
    fn parses_threadtime_with_uid() {
        let line = "07-20 15:43:53.405  1047 17520 18175 I AncSDK  : key frame id: 0";
        let (e, f) = parse_line(line.to_string());
        assert_eq!(f, LogFormat::ThreadTime);
        assert_eq!(e.date(), "07-20");
        assert_eq!(e.time(), "15:43:53.405");
        assert_eq!(e.pid(), "1047");
        assert_eq!(e.tid(), "17520");
        assert_eq!(e.level, LevelMask::I);
        assert_eq!(e.tag(), "AncSDK");
        assert_eq!(e.message(), "key frame id: 0");
    }

    #[test]
    fn empty_and_whitespace_lines() {
        let (e1, f1) = parse_line(String::new());
        assert_eq!(f1, LogFormat::Unknown);
        assert_eq!(e1.message(), "");

        let (e2, f2) = parse_line("   ".to_string());
        assert_eq!(f2, LogFormat::Unknown);
        // Whitespace-only lines are preserved verbatim as Unknown entries
        assert_eq!(e2.message(), "   ");
    }

    #[test]
    fn kernel_level_digit_boundaries() {
        // <0> = Fatal
        let (e, f) = parse_line("<0>[0.001] critical".to_string());
        assert_eq!(f, LogFormat::Kernel);
        assert_eq!(e.level, LevelMask::F);
        // <4> = Warning
        let (e, f) = parse_line("<4>[0.002] warn msg".to_string());
        assert_eq!(f, LogFormat::Kernel);
        assert_eq!(e.level, LevelMask::W);
        // <7> = Debug
        let (e, f) = parse_line("<7>[0.003] debug msg".to_string());
        assert_eq!(f, LogFormat::Kernel);
        assert_eq!(e.level, LevelMask::D);
    }
}
