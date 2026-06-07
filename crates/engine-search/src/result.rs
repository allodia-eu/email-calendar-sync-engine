//! Search result types.
//!
//! A search answer is a ranked list of object keys plus the [`SearchCoverage`]
//! that says how complete it is. The keys are provider object keys the host
//! resolves to full objects through the store; the engine returns identity and
//! rank, not payloads, so large content never crosses an unnecessary boundary
//! (`north-star.md` cross-platform notes). These types are store-agnostic: any
//! backend's executor produces them.

use engine_core::coverage::SearchCoverage;
use engine_core::ids::ProviderKey;

/// One ranked search hit: an object key and its fused rank score.
///
/// `score` is the reciprocal-rank-fusion score across the candidate sources that
/// matched it (full-text, and later vector); higher ranks first. For a pure
/// structured query with no text or vector signal the order is the executor's
/// deterministic fallback (mail by date, calendar by key) and `score` is `0.0`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The matched object's provider key.
    pub key: ProviderKey,
    /// The fused rank score (higher first).
    pub score: f64,
}

impl SearchHit {
    /// Creates a hit.
    #[must_use]
    pub fn new(key: ProviderKey, score: f64) -> Self {
        Self { key, score }
    }
}

/// A ranked search answer with its coverage.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// The hits, best-ranked first.
    pub hits: Vec<SearchHit>,
    /// How complete the answer is, and why it might miss matches.
    pub coverage: SearchCoverage,
}

impl SearchResults {
    /// Creates a result set.
    #[must_use]
    pub fn new(hits: Vec<SearchHit>, coverage: SearchCoverage) -> Self {
        Self { hits, coverage }
    }

    /// The matched keys in rank order, dropping the scores.
    #[must_use]
    pub fn keys(&self) -> Vec<ProviderKey> {
        self.hits.iter().map(|h| h.key.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn results_expose_keys_in_rank_order() {
        let results = SearchResults::new(
            vec![SearchHit::new(key("b"), 0.5), SearchHit::new(key("a"), 0.2)],
            SearchCoverage::complete(),
        );
        assert_eq!(results.keys(), vec![key("b"), key("a")]);
        assert!(results.coverage.is_complete());
    }
}
