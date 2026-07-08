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
            0 | 1 | 2 => Self::F,
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

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub line_no: u32,
    pub date: String,
    pub time: String,
    pub level: LevelMask,
    pub pid: String,
    pub tid: String,
    pub tag: String,
    pub message: String,
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
        entry.line_no = (self.entries.len() as u32) + 1;
        if entry.level.contains(LevelMask::E) || entry.level.contains(LevelMask::F) {
            self.error_lines.push(entry.line_no - 1);
        }
        if !entry.pid.is_empty() {
            *self.pid_counts.entry(entry.pid.clone()).or_insert(0) += 1;
        }
        if !entry.tid.is_empty() {
            *self.tid_counts.entry(entry.tid.clone()).or_insert(0) += 1;
        }
        if !entry.tag.is_empty() {
            *self.tag_counts.entry(entry.tag.clone()).or_insert(0) += 1;
        }
        if let Some(idx) = level_index(entry.level) {
            self.level_counts[idx] += 1;
        }
        self.entries.push(entry);
    }
}

pub fn level_index(lv: LevelMask) -> Option<usize> {
    if lv.contains(LevelMask::V) { Some(0) }
    else if lv.contains(LevelMask::D) { Some(1) }
    else if lv.contains(LevelMask::I) { Some(2) }
    else if lv.contains(LevelMask::W) { Some(3) }
    else if lv.contains(LevelMask::E) { Some(4) }
    else if lv.contains(LevelMask::F) { Some(5) }
    else { None }
}

pub const LEVEL_MASKS: [LevelMask; 6] = [
    LevelMask::V, LevelMask::D, LevelMask::I, LevelMask::W, LevelMask::E, LevelMask::F,
];
