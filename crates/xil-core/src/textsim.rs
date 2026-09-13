//! Python's `difflib` — Ratcliff/Obershelp similarity and
//! `get_close_matches`.
//!
//! The scanner's near-miss cutoff (0.72) is a behavioural boundary, so the
//! ratio has to agree with CPython to the last digit rather than merely be
//! "a similarity measure". This is a direct transcription of
//! `SequenceMatcher`, not an equivalent algorithm.
//!
//! Autojunk is on, as it is by default in CPython: once *b* reaches 200
//! elements, any element occurring more than `len(b) // 100 + 1` times is
//! dropped from the index. That rarely touches a scanner tag but decides
//! the score of a long line of dialogue (0.295 with it, 0.873 without, for
//! one pair of 220-character sentences).

use std::collections::HashMap;

/// `difflib.SequenceMatcher(None, a, b)` over character sequences.
pub struct SequenceMatcher {
    a: Vec<char>,
    b: Vec<char>,
    /// Each char of `b` to the indices where it occurs, in order.
    b2j: HashMap<char, Vec<usize>>,
}

impl SequenceMatcher {
    pub fn new(a: &str, b: &str) -> SequenceMatcher {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
        for (j, c) in b.iter().enumerate() {
            b2j.entry(*c).or_default().push(j);
        }
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        SequenceMatcher { a, b, b2j }
    }

    /// `find_longest_match(alo, ahi, blo, bhi)`. With no `isjunk` the
    /// junk set is empty, so only CPython's first pair of extension passes
    /// can move — and they do, over popular elements autojunk dropped.
    fn find_longest_match(
        &self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(js) = self.b2j.get(&self.a[i]) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|jm| j2len.get(&jm).copied())
                        .unwrap_or(0)
                        + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // Extend the match outward over equal elements.
        while besti > alo && bestj > blo && self.a[besti - 1] == self.b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    /// Total size of all matching blocks — everything `ratio` needs.
    fn total_matches(&self) -> usize {
        let mut queue = vec![(0usize, self.a.len(), 0usize, self.b.len())];
        let mut matches = 0usize;
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k == 0 {
                continue;
            }
            matches += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
        matches
    }

    /// `ratio()` — `2 * matches / (len(a) + len(b))`, or 1.0 for two empties.
    pub fn ratio(&self) -> f64 {
        let length = self.a.len() + self.b.len();
        if length == 0 {
            return 1.0;
        }
        2.0 * self.total_matches() as f64 / length as f64
    }
}

/// `difflib.get_close_matches(word, possibilities, n, cutoff)`.
///
/// CPython sets `word` as sequence *b* and each possibility as *a*, then
/// takes the `n` largest `(ratio, possibility)` tuples — so a tie is broken
/// by the possibility that sorts later.
pub fn get_close_matches(
    word: &str,
    possibilities: &[String],
    n: usize,
    cutoff: f64,
) -> Vec<String> {
    let mut scored: Vec<(f64, &String)> = possibilities
        .iter()
        .map(|x| (SequenceMatcher::new(x, word).ratio(), x))
        .filter(|(r, _)| *r >= cutoff)
        .collect();
    // heapq.nlargest on (score, value) tuples: descending by score, then by
    // value. A stable sort on the reversed comparison reproduces it.
    scored.sort_by(|l, r| {
        r.0.partial_cmp(&l.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| r.1.cmp(l.1))
    });
    scored.into_iter().take(n).map(|(_, x)| x.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ratios recorded from CPython's difflib, printed at 17 significant
    /// digits so a drift in the last bit would show.
    ///
    /// The literals are pasted from that output unedited — trimming them to
    /// the shortest round-tripping form would hide where they came from, so
    /// the precision lint is off for this table alone.
    #[test]
    #[allow(clippy::excessive_precision)]
    fn ratio_matches_cpython() {
        let cases = [
            ("laugh", "laughs", 0.90909090909090906),
            ("clear throat", "clears throat", 0.95999999999999996),
            ("surprised", "surprise", 0.94117647058823528),
            ("narration", "narrating", 0.88888888888888884),
            ("crying", "cries", 0.54545454545454541),
            ("sigh", "sighs", 0.88888888888888884),
            ("cough", "coughs", 0.90909090909090906),
            ("gasp", "gasps", 0.88888888888888884),
            ("angry", "angry", 1.0),
            ("happy", "exhausted", 0.2857142857142857),
            ("chuckle", "chuckles", 0.93333333333333335),
        ];
        for (a, b, want) in cases {
            let got = SequenceMatcher::new(a, b).ratio();
            assert_eq!(got, want, "ratio({a:?}, {b:?}) = {got:.17} want {want:.17}");
        }
    }

    #[test]
    fn autojunk_engages_at_two_hundred() {
        let a = "the quick brown fox jumps over the lazy dog and then keeps running through the forest until it reaches the river where it stops to drink some water before heading back home to its den in the hills beyond the valley far away";
        let b = "the quick brown fox jumped over a lazy dog and kept running through the forest till it reached the river where it stopped to drink water before heading home to its den in the hills past the valley very far away indeed";
        assert_eq!(SequenceMatcher::new(a, b).ratio(), 0.29545454545454547);
        assert_eq!(SequenceMatcher::new(b, a).ratio(), 0.4409090909090909);
    }

    #[test]
    fn empty_sequences_are_identical() {
        assert_eq!(SequenceMatcher::new("", "").ratio(), 1.0);
        assert_eq!(SequenceMatcher::new("", "abc").ratio(), 0.0);
        assert_eq!(SequenceMatcher::new("abc", "").ratio(), 0.0);
    }

    #[test]
    fn close_matches_honour_the_cutoff() {
        let cands: Vec<String> = [
            "laugh",
            "chuckle",
            "sigh",
            "surprised",
            "clear throat",
            "narration",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(get_close_matches("laughs", &cands, 1, 0.72), vec!["laugh"]);
        assert_eq!(
            get_close_matches("clears throat", &cands, 1, 0.72),
            vec!["clear throat"]
        );
        assert_eq!(
            get_close_matches("surprise", &cands, 1, 0.72),
            vec!["surprised"]
        );
        assert!(get_close_matches("exhausted", &cands, 1, 0.72).is_empty());
        assert!(get_close_matches("", &cands, 1, 0.72).is_empty());
    }

    #[test]
    fn ties_break_on_the_later_candidate() {
        // "ab" scores the same against both; CPython's nlargest on the
        // (score, value) tuple keeps the greater string.
        let cands: Vec<String> = vec!["aX".into(), "aY".into()];
        assert_eq!(get_close_matches("ab", &cands, 1, 0.4), vec!["aY"]);
    }

    #[test]
    fn n_limits_the_result() {
        let cands: Vec<String> = vec!["laugh".into(), "laughing".into(), "cough".into()];
        assert_eq!(get_close_matches("laughs", &cands, 2, 0.5).len(), 2);
        assert_eq!(get_close_matches("laughs", &cands, 1, 0.5), vec!["laugh"]);
    }
}
