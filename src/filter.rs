use crate::model::{LevelMask, LogEntry};
use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct FilterSpec {
    /// None = every level passes; Some(mask) = only levels in mask pass.
    pub allowed_levels: Option<LevelMask>,
    /// None = every value passes; Some(set) = only values in set pass.
    pub allowed_pids: Option<HashSet<String>>,
    pub allowed_tids: Option<HashSet<String>>,
    pub allowed_tags: Option<HashSet<String>>,
    /// Tags explicitly excluded via Alt+right-click. Takes effect even when
    /// `allowed_tags` is None (all-pass), so newly streamed tags are also
    /// excluded without needing to snapshot the full tag set at click time.
    pub disallowed_tags: HashSet<String>,

    pub find: Vec<String>,
    pub remove: Vec<String>,

    pub bookmarks_only: bool,
    pub errors_only: bool,
}

impl FilterSpec {
    /// Split a raw text field on `|` into lowercased trimmed tokens.
    pub fn tokens(raw: &str) -> Vec<String> {
        raw.split('|')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    }

    pub fn matches(&self, entry: &LogEntry, entry_idx: u32, bookmarks: &HashSet<u32>) -> bool {
        if let Some(mask) = self.allowed_levels {
            if !mask.intersects(entry.level) {
                return false;
            }
        }
        if let Some(set) = &self.allowed_pids {
            if !set.contains(entry.pid()) {
                return false;
            }
        }
        if let Some(set) = &self.allowed_tids {
            if !set.contains(entry.tid()) {
                return false;
            }
        }
        if let Some(set) = &self.allowed_tags {
            if !set.contains(entry.tag()) {
                return false;
            }
        }
        if !self.disallowed_tags.is_empty() && self.disallowed_tags.contains(entry.tag()) {
            return false;
        }
        if self.bookmarks_only && !bookmarks.contains(&entry_idx) {
            return false;
        }
        if self.errors_only
            && !(entry.level.contains(LevelMask::E) || entry.level.contains(LevelMask::F))
        {
            return false;
        }
        if !self.find.is_empty() && !any_contains(entry.message(), &self.find) {
            return false;
        }
        if !self.remove.is_empty() && any_contains(entry.message(), &self.remove) {
            return false;
        }
        true
    }
}

fn any_contains(hay: &str, needles: &[String]) -> bool {
    needles.iter().any(|n| contains_ci(hay, n))
}

/// Case-insensitive substring search. `needle` is assumed already lowercased
/// (see [`FilterSpec::tokens`]). For the common all-ASCII needle we fold ASCII
/// letters of `hay` on the fly and scan byte-wise — no allocation. UTF-8
/// continuation bytes (>= 0x80) fold to themselves and never equal an ASCII
/// needle byte, so multibyte text is handled correctly. A needle containing
/// non-ASCII falls back to Unicode-correct lowercasing.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if !needle.is_ascii() {
        return hay.to_lowercase().contains(needle);
    }
    let hay = hay.as_bytes();
    let nee = needle.as_bytes();
    if nee.len() > hay.len() {
        return false;
    }
    'outer: for start in 0..=hay.len() - nee.len() {
        for (j, &nb) in nee.iter().enumerate() {
            if hay[start + j].to_ascii_lowercase() != nb {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LevelMask;

    fn e(msg: &str, tag: &str, lv: LevelMask) -> LogEntry {
        LogEntry::from_fields("", "", lv, "1", "1", tag, msg)
    }

    #[test]
    fn tokens_split_and_lowercase() {
        assert_eq!(FilterSpec::tokens("Foo | Bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn level_mask_filters() {
        let spec = FilterSpec {
            allowed_levels: Some(LevelMask::E),
            ..FilterSpec::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("x", "T", LevelMask::E), 0, &hs));
        assert!(!spec.matches(&e("x", "T", LevelMask::D), 0, &hs));
    }

    #[test]
    fn find_or_remove() {
        let spec = FilterSpec {
            find: vec!["hello".into()],
            ..FilterSpec::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("Hello world", "T", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("bye", "T", LevelMask::I), 0, &hs));

        let spec = FilterSpec {
            remove: vec!["spam".into()],
            ..FilterSpec::default()
        };
        assert!(!spec.matches(&e("spam here", "T", LevelMask::I), 0, &hs));
        assert!(spec.matches(&e("clean", "T", LevelMask::I), 0, &hs));
    }

    #[test]
    fn contains_ci_cases() {
        // ASCII case-insensitive, no allocation path
        assert!(contains_ci("Hello World", "hello"));
        assert!(contains_ci("ERROR: boom", "error"));
        assert!(contains_ci("abcABC", "cabc"));
        assert!(!contains_ci("abc", "xyz"));
        assert!(!contains_ci("ab", "abc")); // needle longer than hay
        assert!(contains_ci("anything", "")); // empty needle matches
                                              // Multibyte haystack must not corrupt the byte scan
        assert!(contains_ci("日志Error信息", "error"));
        assert!(!contains_ci("日志信息", "error"));
        // Non-ASCII needle falls back to Unicode lowercasing
        assert!(contains_ci("包含中文Log", "中文"));
    }

    #[test]
    fn allowed_pids_filters() {
        let mut spec = FilterSpec::default();
        let mut set = HashSet::new();
        set.insert("1".to_string());
        spec.allowed_pids = Some(set);
        let hs = HashSet::new();
        assert!(spec.matches(&e("m", "T", LevelMask::I), 0, &hs));
        let other = LogEntry::from_fields("", "", LevelMask::I, "99", "1", "T", "m");
        assert!(!spec.matches(&other, 0, &hs));
    }

    #[test]
    fn allowed_tags_filters() {
        let spec = FilterSpec {
            allowed_tags: Some(["OK".into()].into()),
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("m", "OK", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("m", "BAD", LevelMask::I), 0, &hs));
    }

    #[test]
    fn allowed_tids_filters() {
        let spec = FilterSpec {
            allowed_tids: Some(["200".into()].into()),
            ..Default::default()
        };
        let hs = HashSet::new();
        let entry = LogEntry::from_fields("", "", LevelMask::I, "1", "200", "T", "m");
        assert!(spec.matches(&entry, 0, &hs));
        let other = LogEntry::from_fields("", "", LevelMask::I, "1", "999", "T", "m");
        assert!(!spec.matches(&other, 0, &hs));
    }

    #[test]
    fn disallowed_tags_rejects() {
        let spec = FilterSpec {
            disallowed_tags: ["BAD".into()].into(),
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(!spec.matches(&e("m", "BAD", LevelMask::I), 0, &hs));
        assert!(spec.matches(&e("m", "OK", LevelMask::I), 0, &hs));
    }

    #[test]
    fn disallowed_plus_allowed_tags() {
        let spec = FilterSpec {
            allowed_tags: Some(["A".into(), "B".into()].into()),
            disallowed_tags: ["B".into()].into(),
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("m", "A", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("m", "B", LevelMask::I), 0, &hs));
    }

    #[test]
    fn bookmarks_only_filters() {
        let spec = FilterSpec {
            bookmarks_only: true,
            ..Default::default()
        };
        let mut bm = HashSet::new();
        bm.insert(5u32);
        assert!(spec.matches(&e("m", "T", LevelMask::I), 5, &bm));
        assert!(!spec.matches(&e("m", "T", LevelMask::I), 3, &bm));
    }

    #[test]
    fn errors_only_filters() {
        let spec = FilterSpec {
            errors_only: true,
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("m", "T", LevelMask::E), 0, &hs));
        assert!(spec.matches(&e("m", "T", LevelMask::F), 0, &hs));
        assert!(!spec.matches(&e("m", "T", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("m", "T", LevelMask::W), 0, &hs));
    }

    #[test]
    fn multiple_find_tokens_or_semantics() {
        let spec = FilterSpec {
            find: vec!["foo".into(), "bar".into()],
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("contains foo here", "T", LevelMask::I), 0, &hs));
        assert!(spec.matches(&e("has bar inside", "T", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("neither word", "T", LevelMask::I), 0, &hs));
    }

    #[test]
    fn multiple_remove_tokens_or_semantics() {
        let spec = FilterSpec {
            remove: vec!["spam".into(), "ads".into()],
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(!spec.matches(&e("spam message", "T", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("has ads", "T", LevelMask::I), 0, &hs));
        assert!(spec.matches(&e("clean line", "T", LevelMask::I), 0, &hs));
    }

    #[test]
    fn find_and_remove_combined() {
        let spec = FilterSpec {
            find: vec!["hello".into()],
            remove: vec!["hello world".into()],
            ..Default::default()
        };
        let hs = HashSet::new();
        assert!(spec.matches(&e("hello there", "T", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("hello world", "T", LevelMask::I), 0, &hs));
        assert!(!spec.matches(&e("no match", "T", LevelMask::I), 0, &hs));
    }
}
