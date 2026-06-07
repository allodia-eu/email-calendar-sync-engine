//! Reciprocal-rank fusion (RRF).
//!
//! When a query produces several ranked candidate lists — FTS `bm25()` hits and
//! vector-KNN hits — they must merge into one ranking without comparing scores
//! across incompatible scales. RRF does this with rank alone: a candidate's score
//! is the sum over the lists it appears in of `1 / (k + rank)`, with `rank`
//! 1-based and `k` a smoothing constant (the conventional default is 60). A
//! candidate near the top of several lists outranks one that tops a single list.
//!
//! This is store-agnostic and generic over the candidate key (a provider key, a
//! rowid, …), so the same fusion runs over any store's native candidates
//! (`search-coverage.md`, `north-star.md` Search Contract).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;

/// The RRF smoothing constant `k`.
///
/// Larger `k` flattens the contribution curve (later ranks matter relatively
/// more); the widely used default is 60.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RrfK(u32);

impl RrfK {
    /// The conventional default, `k = 60`.
    pub const DEFAULT: RrfK = RrfK(60);

    /// Creates a smoothing constant.
    #[must_use]
    pub fn new(k: u32) -> Self {
        Self(k)
    }

    /// Returns the constant's value.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for RrfK {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A fused candidate: its key and accumulated RRF score.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused<K> {
    /// The candidate key.
    pub key: K,
    /// The accumulated reciprocal-rank score (higher ranks first).
    pub score: f64,
}

/// Fuses ranked candidate lists into one ranking by reciprocal-rank fusion.
///
/// Each input list is ordered best-first (rank 1 is the first element). A key's
/// score is `Σ 1 / (k + rank)` over every list it appears in. The result is
/// sorted by descending score; ties break by first appearance across the input
/// lists, so fusion is deterministic regardless of [`HashMap`] iteration order.
///
/// A key repeated *within* one list contributes once per occurrence — callers
/// pass de-duplicated lists, which every store-native candidate source produces.
#[must_use]
pub fn fuse<K: Eq + Hash + Clone>(lists: &[&[K]], k: RrfK) -> Vec<Fused<K>> {
    let smoothing = f64::from(k.get());
    // Value: (accumulated score, first-seen ordinal for stable tie-breaking).
    let mut scores: HashMap<K, (f64, usize)> = HashMap::new();
    for list in lists {
        for (index, key) in list.iter().enumerate() {
            let rank = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let contribution = 1.0 / (smoothing + f64::from(rank));
            let order = scores.len();
            match scores.entry(key.clone()) {
                Entry::Occupied(mut entry) => entry.get_mut().0 += contribution,
                Entry::Vacant(slot) => {
                    slot.insert((contribution, order));
                }
            }
        }
    }
    let mut fused: Vec<(K, f64, usize)> = scores
        .into_iter()
        .map(|(key, (score, order))| (key, score, order))
        .collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    fused
        .into_iter()
        .map(|(key, score, _)| Fused { key, score })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys<'a>(fused: &[Fused<&'a str>]) -> Vec<&'a str> {
        fused.iter().map(|f| f.key).collect()
    }

    #[test]
    fn k_constant_accessors() {
        assert_eq!(RrfK::default(), RrfK::DEFAULT);
        assert_eq!(RrfK::DEFAULT.get(), 60);
        assert_eq!(RrfK::new(10).get(), 10);
    }

    #[test]
    fn empty_input_fuses_to_nothing() {
        let fused = fuse::<&str>(&[], RrfK::DEFAULT);
        assert!(fused.is_empty());
        let fused = fuse::<&str>(&[&[][..]], RrfK::DEFAULT);
        assert!(fused.is_empty());
    }

    #[test]
    fn a_single_list_preserves_its_order_with_decreasing_scores() {
        let list = ["a", "b", "c"];
        let fused = fuse(&[&list[..]], RrfK::DEFAULT);
        assert_eq!(keys(&fused), vec!["a", "b", "c"]);
        assert!(fused[0].score > fused[1].score);
        assert!(fused[1].score > fused[2].score);
        // 1 / (60 + 1) for the top candidate.
        assert!((fused[0].score - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn two_lists_fuse_by_summed_reciprocal_rank() {
        // a: 1/61 ; b: 1/62 + 1/61 ; c: 1/63 + 1/62 ; d: 1/63
        let one = ["a", "b", "c"];
        let two = ["b", "c", "d"];
        let fused = fuse(&[&one[..], &two[..]], RrfK::DEFAULT);
        // b and c appear in both lists, so they outrank the singly-listed a, d.
        assert_eq!(keys(&fused), vec!["b", "c", "a", "d"]);
        let b = 1.0 / 62.0 + 1.0 / 61.0;
        assert!((fused[0].score - b).abs() < 1e-12);
    }

    #[test]
    fn ties_break_by_first_appearance() {
        // a and b each appear once at rank 1, so scores are equal; a was seen
        // first (in the first list), so it sorts first regardless of map order.
        let one = ["a"];
        let two = ["b"];
        let fused = fuse(&[&one[..], &two[..]], RrfK::DEFAULT);
        assert_eq!(keys(&fused), vec!["a", "b"]);
        assert!((fused[0].score - fused[1].score).abs() < 1e-12);
    }

    #[test]
    fn smoothing_constant_changes_the_score_scale() {
        let list = ["a"];
        let small = fuse(&[&list[..]], RrfK::new(0));
        let large = fuse(&[&list[..]], RrfK::new(1000));
        // k = 0 → 1/1 = 1.0 ; larger k shrinks the contribution.
        assert!((small[0].score - 1.0).abs() < 1e-12);
        assert!(large[0].score < small[0].score);
    }
}
