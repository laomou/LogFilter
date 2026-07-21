use bitflags::bitflags;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct LevelMask: u8 {
        const V = 1 << 0;
        const D = 1 << 1;
        const I = 1 << 2;
        const W = 1 << 3;
        const E = 1 << 4;
        const F = 1 << 5;
    }
}

impl LevelMask {
    pub const ALL: LevelMask = LevelMask::all();

    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'V' | 'v' => Some(Self::V),
            'D' | 'd' => Some(Self::D),
            'I' | 'i' => Some(Self::I),
            'W' | 'w' => Some(Self::W),
            'E' | 'e' => Some(Self::E),
            'F' | 'f' | 'A' | 'a' => Some(Self::F),
            _ => None,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            Self::V => 'V',
            Self::D => 'D',
            Self::I => 'I',
            Self::W => 'W',
            Self::E => 'E',
            Self::F => 'F',
            _ => '?',
        }
    }

    pub fn from_kernel_digit(d: u8) -> Self {
        match d {
            0..=2 => Self::F,
            3 => Self::E,
            4 => Self::W,
            5 | 6 => Self::I,
            _ => Self::D,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Brief,
    Time,
    ThreadTime,
    Kernel,
    Unknown,
}

/// Byte range `[start, end)` into a [`LogEntry::raw`] line.
type Span = (u32, u32);

/// One parsed log line. To keep memory low on large files, the whole original
/// line is stored once as a single `Box<str>` and each field is a byte range
/// into it (accessed via the methods below) — one allocation per entry instead
/// of six separate `String`s.
#[derive(Debug, Clone)]
pub struct LogEntry {
    raw: Box<str>,
    pub line_no: u64,
    pub level: LevelMask,
    date: Span,
    time: Span,
    pid: Span,
    tid: Span,
    tag: Span,
    message: Span,
}

impl LogEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        raw: Box<str>,
        level: LevelMask,
        date: Span,
        time: Span,
        pid: Span,
        tid: Span,
        tag: Span,
        message: Span,
    ) -> Self {
        Self {
            raw,
            line_no: 0u64,
            level,
            date,
            time,
            pid,
            tid,
            tag,
            message,
        }
    }

    #[inline]
    fn slice(&self, s: Span) -> &str {
        &self.raw[s.0 as usize..s.1 as usize]
    }

    #[inline]
    pub fn date(&self) -> &str {
        self.slice(self.date)
    }
    #[inline]
    pub fn time(&self) -> &str {
        self.slice(self.time)
    }
    #[inline]
    pub fn pid(&self) -> &str {
        self.slice(self.pid)
    }
    #[inline]
    pub fn tid(&self) -> &str {
        self.slice(self.tid)
    }
    #[inline]
    pub fn tag(&self) -> &str {
        self.slice(self.tag)
    }
    #[inline]
    pub fn message(&self) -> &str {
        self.slice(self.message)
    }

    /// Build an entry from separate field strings (tests / synthetic data):
    /// concatenates the fields into one backing buffer and records their spans.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_fields(
        date: &str,
        time: &str,
        level: LevelMask,
        pid: &str,
        tid: &str,
        tag: &str,
        message: &str,
    ) -> Self {
        let mut raw = String::new();
        let push = |raw: &mut String, s: &str| {
            let start = raw.len() as u32;
            raw.push_str(s);
            (start, raw.len() as u32)
        };
        let d = push(&mut raw, date);
        let t = push(&mut raw, time);
        let p = push(&mut raw, pid);
        let i = push(&mut raw, tid);
        let g = push(&mut raw, tag);
        let m = push(&mut raw, message);
        Self {
            raw: raw.into_boxed_str(),
            line_no: 1u64,
            level,
            date: d,
            time: t,
            pid: p,
            tid: i,
            tag: g,
            message: m,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncodingChoice {
    #[default]
    Utf8,
    Local,
}

#[derive(Debug, Default)]
pub struct Model {
    pub entries: Vec<LogEntry>,
    pub filtered: Vec<u32>,
    pub bookmarks: HashSet<u32>,
    pub error_lines: Vec<u32>,
    pub file_path: Option<PathBuf>,

    // Cached distinct-value counts for column picker panels.
    pub pid_counts: HashMap<String, usize>,
    pub tid_counts: HashMap<String, usize>,
    pub tag_counts: HashMap<String, usize>,
    pub level_counts: [usize; 6], // V/D/I/W/E/F
}

impl Model {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.filtered.clear();
        self.bookmarks.clear();
        self.error_lines.clear();
        self.file_path = None;
        self.pid_counts.clear();
        self.tid_counts.clear();
        self.tag_counts.clear();
        self.level_counts = [0; 6];
    }

    pub fn append(&mut self, mut entry: LogEntry) {
        entry.line_no = (self.entries.len() as u64) + 1;
        if entry.level.contains(LevelMask::E) || entry.level.contains(LevelMask::F) {
            self.error_lines.push(entry.line_no as u32 - 1);
        }
        // Bump per-value counts. Clone the key only when it's a new distinct
        // value (cardinality is tiny), not once per line.
        bump_count(&mut self.pid_counts, entry.pid());
        bump_count(&mut self.tid_counts, entry.tid());
        bump_count(&mut self.tag_counts, entry.tag());
        if let Some(idx) = level_index(entry.level) {
            self.level_counts[idx] += 1;
        }
        self.entries.push(entry);
    }
}

/// Increment `map[key]`, allocating the key string only on first insert.
fn bump_count(map: &mut HashMap<String, usize>, key: &str) {
    if key.is_empty() {
        return;
    }
    if let Some(v) = map.get_mut(key) {
        *v += 1;
    } else {
        map.insert(key.to_string(), 1);
    }
}

pub fn level_index(lv: LevelMask) -> Option<usize> {
    if lv.contains(LevelMask::V) {
        Some(0)
    } else if lv.contains(LevelMask::D) {
        Some(1)
    } else if lv.contains(LevelMask::I) {
        Some(2)
    } else if lv.contains(LevelMask::W) {
        Some(3)
    } else if lv.contains(LevelMask::E) {
        Some(4)
    } else if lv.contains(LevelMask::F) {
        Some(5)
    } else {
        None
    }
}

pub const LEVEL_MASKS: [LevelMask; 6] = [
    LevelMask::V,
    LevelMask::D,
    LevelMask::I,
    LevelMask::W,
    LevelMask::E,
    LevelMask::F,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_sets_line_no_sequentially() {
        let mut m = Model::default();
        for _ in 0..3 {
            m.append(LogEntry::from_fields(
                "",
                "",
                LevelMask::I,
                "1",
                "1",
                "T",
                "x",
            ));
        }
        assert_eq!(m.entries[0].line_no, 1);
        assert_eq!(m.entries[1].line_no, 2);
        assert_eq!(m.entries[2].line_no, 3);
    }

    #[test]
    fn append_tracks_error_lines() {
        let mut m = Model::default();
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::I,
            "1",
            "1",
            "T",
            "info",
        ));
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::E,
            "1",
            "1",
            "T",
            "err",
        ));
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::W,
            "1",
            "1",
            "T",
            "warn",
        ));
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::F,
            "1",
            "1",
            "T",
            "fatal",
        ));
        // error_lines stores entry indices of E/F entries
        assert_eq!(m.error_lines, vec![1, 3]);
    }

    #[test]
    fn append_bumps_counts() {
        let mut m = Model::default();
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::I,
            "100",
            "200",
            "TagA",
            "x",
        ));
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::I,
            "100",
            "200",
            "TagA",
            "x",
        ));
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::E,
            "100",
            "300",
            "TagB",
            "x",
        ));
        assert_eq!(m.pid_counts["100"], 3);
        assert_eq!(m.tid_counts["200"], 2);
        assert_eq!(m.tid_counts["300"], 1);
        assert_eq!(m.tag_counts["TagA"], 2);
        assert_eq!(m.tag_counts["TagB"], 1);
        assert_eq!(m.level_counts[2], 2); // I
        assert_eq!(m.level_counts[4], 1); // E
    }

    #[test]
    fn clear_resets_everything() {
        let mut m = Model::default();
        m.append(LogEntry::from_fields(
            "",
            "",
            LevelMask::E,
            "1",
            "1",
            "T",
            "x",
        ));
        m.bookmarks.insert(0);
        m.filtered.push(0);
        m.clear();
        assert!(m.entries.is_empty());
        assert!(m.filtered.is_empty());
        assert!(m.bookmarks.is_empty());
        assert!(m.error_lines.is_empty());
        assert!(m.pid_counts.is_empty());
        assert_eq!(m.level_counts, [0; 6]);
    }

    #[test]
    fn level_mask_from_char_variants() {
        assert_eq!(LevelMask::from_char('V'), Some(LevelMask::V));
        assert_eq!(LevelMask::from_char('v'), Some(LevelMask::V));
        assert_eq!(LevelMask::from_char('E'), Some(LevelMask::E));
        assert_eq!(LevelMask::from_char('A'), Some(LevelMask::F)); // alias
        assert_eq!(LevelMask::from_char('a'), Some(LevelMask::F));
        assert_eq!(LevelMask::from_char('X'), None);
    }

    #[test]
    fn level_index_maps_correctly() {
        assert_eq!(level_index(LevelMask::V), Some(0));
        assert_eq!(level_index(LevelMask::D), Some(1));
        assert_eq!(level_index(LevelMask::I), Some(2));
        assert_eq!(level_index(LevelMask::W), Some(3));
        assert_eq!(level_index(LevelMask::E), Some(4));
        assert_eq!(level_index(LevelMask::F), Some(5));
        assert_eq!(level_index(LevelMask::empty()), None);
    }
}
