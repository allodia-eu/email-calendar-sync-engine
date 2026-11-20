//! What storing a draft answers.

use std::collections::BTreeSet;

use engine_core::{ids::ProviderKey, mail::Keyword};

/// The result of storing a draft ([`Provider::put_draft`](crate::Provider::put_draft)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftReceipt {
    /// The key the draft is now stored under. Not necessarily the one it replaced: keep this
    /// one and pass it as `replacing` on the next save.
    pub key: ProviderKey,
    /// Which of the keywords the save asked for the stored draft carries.
    ///
    /// Empty when the save asked for none, and when the adapter or the server could not keep
    /// them. A keyword the save asked for and this set lacks was **not** stored; the save
    /// itself is unaffected. Whether an adapter tries at all is
    /// [`Capabilities::draft_keywords`](crate::Capabilities::draft_keywords).
    pub keywords: BTreeSet<Keyword>,
}

impl DraftReceipt {
    /// Records a draft stored under `key`, carrying no keyword.
    #[must_use]
    pub fn stored(key: ProviderKey) -> Self {
        Self {
            key,
            keywords: BTreeSet::new(),
        }
    }

    /// Records the keywords the stored draft carries ([`Self::keywords`]).
    #[must_use]
    pub fn with_keywords(mut self, keywords: BTreeSet<Keyword>) -> Self {
        self.keywords = keywords;
        self
    }
}
