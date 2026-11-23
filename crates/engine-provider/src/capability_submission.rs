//! The submission half of [`Capabilities`] beyond sending itself: what an adapter does
//! with the sender's filed copy, and with a stored draft's keywords.

use crate::Capabilities;

impl Capabilities {
    /// Marks setting a draft's [`sent_copy_metadata`](crate::Draft::sent_copy_metadata) on
    /// the filed copy as supported.
    ///
    /// The adapter asks the server for them; the server may still decline (an IMAP folder
    /// that does not allow new keywords), and a provider that keeps a keyword only as a named
    /// label or category keeps only those it has a [`KeywordName`](crate::KeywordName) for,
    /// so what was kept is per send, in
    /// [`SubmissionReceipt::sent_copy_kept`](crate::SubmissionReceipt::sent_copy_kept).
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

    /// Marks keeping keywords on a stored draft as supported
    /// ([`Provider::put_draft`](crate::Provider::put_draft)'s `keywords`).
    ///
    /// As for [`with_sent_copy_keywords`](Self::with_sent_copy_keywords), the server may still
    /// decline, and a provider that keeps a keyword only as a named label or category keeps
    /// only those it has a [`KeywordName`](crate::KeywordName) for, so what was kept is per
    /// save, in [`DraftReceipt::kept`](crate::DraftReceipt::kept). Without this flag
    /// the draft is stored without them and the receipt says so.
    #[must_use]
    pub const fn with_draft_keywords(mut self) -> Self {
        self.draft_keywords = true;
        self
    }

    /// Whether the adapter keeps keywords on a stored draft.
    #[must_use]
    pub const fn draft_keywords(self) -> bool {
        self.draft_keywords
    }
}
