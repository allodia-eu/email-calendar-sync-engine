//! The submission half of [`Capabilities`] beyond sending itself: what an adapter does
//! with the sender's filed copy.

use crate::Capabilities;

impl Capabilities {
    /// Marks setting a draft's [`sent_copy_keywords`](crate::Draft::sent_copy_keywords) on
    /// the filed copy as supported.
    ///
    /// The adapter asks the server for them; the server may still decline (an IMAP folder
    /// that does not allow new keywords), and a provider that keeps a keyword only as a named
    /// label or category keeps only those it has a [`KeywordName`](crate::KeywordName) for,
    /// so what was kept is per send, in
    /// [`SubmissionReceipt::sent_copy_keywords`](crate::SubmissionReceipt::sent_copy_keywords).
    /// Without this flag the copy is filed without them and the receipt says so.
    #[must_use]
    pub const fn with_sent_copy_keywords(mut self) -> Self {
        self.sent_copy_keywords = true;
        self
    }

    /// Whether the adapter sets keywords on the sender's filed copy.
    #[must_use]
    pub const fn sent_copy_keywords(self) -> bool {
        self.sent_copy_keywords
    }
}
