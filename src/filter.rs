use crate::model::{LevelMask, LogEntry};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
}

impl FilterSpec {
    pub fn is_passthrough(&self) -> bool {
        self.allowed_levels.is_none()
            && self.allowed_pids.is_none()
            && self.allowed_tids.is_none()
            && self.allowed_tags.is_none()
            && self.disallowed_tags.is_empty()
            && self.find.is_empty()
            && self.remove.is_empty()
            && !self.bookmarks_only
    }

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
    // ASCII needles scan the raw bytes with no allocation. A non-ASCII needle
    // needs the case-folded line; fold it at most once per message so several
    // Unicode tokens share a single allocation instead of each re-folding
    // the whole message.
    let folded: Option<String> = needles
        .iter()
        .any(|needle| !needle.is_ascii())
        .then(|| hay.to_lowercase());
    needles.iter().any(|needle| match folded.as_deref() {
        Some(folded) if !needle.is_ascii() => folded.contains(needle.as_str()),
        _ => contains_ci(hay, needle),
    })
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

    // Microbenchmarks for the search paths `matches` takes. They run inside
    // the regular suite (an external `criterion` target cannot link a
    // binary-only package), print ns/op with `--nocapture`, and fail only on
    // a catastrophic regression — the bounds are orders of magnitude above
    // the expected cost.
    mod benches {
        use super::*;
        use std::time::Instant;

        /// ~4 KiB of realistic mixed-language log text (ASCII + CJK).
        fn mixed_line() -> String {
            let unit =
                "01-01 12:34:56.789  1234  5678 E LogCat: 网络请求失败 retry=3 error=timeout ";
            let mut line = String::with_capacity(4096);
            while line.len() < 4096 {
                line.push_str(unit);
            }
            line.truncate(4096);
            // 4096 may land mid-UTF-8; back off to a char boundary.
            while !line.is_char_boundary(line.len()) {
                line.pop();
            }
            line
        }

        fn time_ops(mut op: impl FnMut()) -> f64 {
            for _ in 0..20 {
                op();
            }
            let start = Instant::now();
            for _ in 0..200 {
                op();
            }
            start.elapsed().as_nanos() as f64 / 200.0
        }

        #[test]
        fn bench_ascii_needle_ascii_line() {
            let line = mixed_line();
            let hit = time_ops(|| assert!(any_contains(&line, &["error".into()])));
            let miss = time_ops(|| assert!(!any_contains(&line, &["zzzabsentneedle".into()])));
            eprintln!("bench ascii needle (4KiB line): hit {hit:.0} ns/op, miss {miss:.0} ns/op");
            // Byte-wise scan: tens of thousands of compares at most.
            assert!(
                miss < 1_000_000.0,
                "ASCII search regressed to {miss:.0} ns/op"
            );
        }

        #[test]
        fn bench_unicode_needle_mixed_line() {
            let line = mixed_line();
            // One Unicode needle must fold the line exactly once.
            let hit = time_ops(|| assert!(any_contains(&line, &["网络".into()])));
            eprintln!("bench unicode needle (4KiB line): hit {hit:.0} ns/op");
            assert!(
                hit < 5_000_000.0,
                "Unicode search regressed to {hit:.0} ns/op"
            );
        }

        #[test]
        fn bench_mixed_needles_share_one_fold() {
            let line = mixed_line();
            // Two ASCII + two Unicode needles. Before the shared-fold fix the
            // line was re-folded once per Unicode needle (two allocations of
            // the whole message); now it is folded once total.
            let needles = vec![
                "error".into(),
                "timeout".into(),
                "网络".into(),
                "失败".into(),
            ];
            let hit = time_ops(|| assert!(any_contains(&line, &needles)));
            eprintln!("bench mixed needles (4KiB line): {hit:.0} ns/op");
            assert!(
                hit < 5_000_000.0,
                "Mixed-needle search regressed to {hit:.0} ns/op"
            );
        }
    }
}
