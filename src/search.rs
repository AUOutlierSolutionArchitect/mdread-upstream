//! Case-insensitive document search mapped onto inline leaves.

use std::collections::HashMap;
use std::ops::Range;

use crate::document::Document;

#[derive(Clone, Debug)]
pub struct Match {
    pub leaf: usize,
    pub block: usize,
    pub range: Range<usize>,
}

#[derive(Default)]
pub struct SearchState {
    pub open: bool,
    pub query: String,
    pub last_query: String,
    pub last_gen: u64,
    pub matches: Vec<Match>,
    pub current: usize,
    /// leaf id → [(match index, byte range)]
    pub by_leaf: HashMap<usize, Vec<(usize, Range<usize>)>>,
    pub focus_pending: bool,
}

impl SearchState {
    pub fn needs_run(&self, gen: u64) -> bool {
        self.open && (self.query != self.last_query || self.last_gen != gen)
    }

    pub fn run(&mut self, doc: &Document, gen: u64) {
        self.last_query = self.query.clone();
        self.last_gen = gen;
        self.matches.clear();
        self.by_leaf.clear();
        self.current = 0;
        let needle = self.query.trim().to_lowercase();
        if needle.is_empty() {
            return;
        }
        let mut matches = Vec::new();
        doc.for_each_leaf(&mut |leaf, block, text| {
            for r in find_ci(text, &needle) {
                matches.push(Match { leaf, block, range: r });
            }
        });
        for (i, m) in matches.iter().enumerate() {
            self.by_leaf.entry(m.leaf).or_default().push((i, m.range.clone()));
        }
        self.matches = matches;
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        }
    }

    pub fn current_block(&self) -> Option<usize> {
        self.matches.get(self.current).map(|m| m.block)
    }

    pub fn clear(&mut self) {
        self.open = false;
        self.matches.clear();
        self.by_leaf.clear();
        self.last_query.clear();
    }
}

/// All non-overlapping case-insensitive occurrences of `needle_lower` in `hay`,
/// as byte ranges into `hay`.
pub fn find_ci(hay: &str, needle_lower: &str) -> Vec<Range<usize>> {
    if needle_lower.is_empty() || hay.len() < needle_lower.len() {
        return Vec::new();
    }
    // Fast path: ASCII haystack means lowercasing preserves byte offsets.
    if hay.is_ascii() {
        let lower = hay.to_ascii_lowercase();
        return lower.match_indices(needle_lower).map(|(s, m)| s..s + m.len()).collect();
    }
    let mut lower = String::with_capacity(hay.len());
    let mut map: Vec<usize> = Vec::with_capacity(hay.len() + 1);
    for (orig, c) in hay.char_indices() {
        for lc in c.to_lowercase() {
            let n = lc.len_utf8();
            for _ in 0..n {
                map.push(orig);
            }
            lower.push(lc);
        }
    }
    map.push(hay.len());
    lower
        .match_indices(needle_lower)
        .map(|(s, m)| {
            let start = map[s];
            let end_lower = s + m.len();
            // end maps to the start of the next original char
            let end = if end_lower >= map.len() - 1 { hay.len() } else { map[end_lower] };
            start..end
        })
        .collect()
}
