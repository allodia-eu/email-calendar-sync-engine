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

    /// Marks the filed copy's keywords as kept **after** the send rather than within it.
    ///
    /// Some transports file the copy on their own a moment after accepting the message, so
    /// the adapter cannot tag it without holding the send's answer until it appears. Such an
    /// adapter keeps none of them in the
    /// [`SubmissionReceipt`](crate::SubmissionReceipt), and the outbox records a
    /// [`MailEdit::SetKeywords`](crate::MailEdit::SetKeywords) on the copy's key for the
    /// drainer to apply, retrying while the copy is not there yet. What the copy carries is
    /// then read from sync.
    #[must_use]
    pub const fn with_sent_copy_keywords_deferred(mut self) -> Self {
        self.sent_copy_keywords_deferred = true;
        self
    }

    /// Whether the filed copy's keywords are kept after the send, by an edit the outbox
    /// records.
    #[must_use]
    pub const fn sent_copy_keywords_deferred(self) -> bool {
        self.sent_copy_keywords_deferred
    }
}
