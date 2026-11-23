//! What storing a draft answers.

use engine_core::{ids::ProviderKey, mail::MessageMetadata};

/// The result of storing a draft ([`Provider::put_draft`](crate::Provider::put_draft)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftReceipt {
    /// The key the draft is now stored under. Not necessarily the one it replaced: keep this
    /// one and pass it as `replacing` on the next save.
    pub key: ProviderKey,
    /// Of the metadata the save asked for, what the stored draft carries.
    ///
    /// Empty when the save asked for nothing, and when the adapter or the server could not
    /// keep it. Whatever the save asked for and this lacks was **not** stored; the save itself
    /// is unaffected. Whether an adapter tries to keep keywords at all is
    /// [`Capabilities::draft_keywords`](crate::Capabilities::draft_keywords); the target stamps
    /// a draft's arrival time itself, so an asked one is never kept.
    pub kept: MessageMetadata,
}

impl DraftReceipt {
    /// Records a draft stored under `key`, carrying no metadata.
    #[must_use]
    pub fn stored(key: ProviderKey) -> Self {
        Self {
            key,
            kept: MessageMetadata::default(),
        }
    }

    /// Records the metadata the stored draft carries ([`Self::kept`]).
    #[must_use]
    pub fn with_kept(mut self, kept: MessageMetadata) -> Self {
        self.kept = kept;
        self
    }
}
