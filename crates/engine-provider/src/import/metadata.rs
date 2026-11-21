//! What a message carries beside its bytes.

use std::collections::BTreeSet;

use engine_core::{mail::Keyword, time::UtcDateTime};

/// What a message carries beside its bytes: the state a target may or may not keep.
///
/// An import asks for it ([`MessageImport::metadata`](super::MessageImport::metadata)), and the
/// answer says what of it the target stored
/// ([`ImportedMessage::kept`](super::ImportedMessage::kept)), so a target that keeps less reports
/// it per message, field by field.
///
/// It follows [`MailState`](engine_core::mail::MailState), the state a sync reads: a field added
/// there (Microsoft Graph's `categories` is the next one) is added here in the same change, so
/// what a source reports maps onto what a target is asked for one to one. Built with
/// [`Default`] and the `with_*` methods, so a field added later does not break a caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MessageMetadata {
    /// The keywords (`$seen`, `$flagged`, and custom ones).
    pub keywords: BTreeSet<Keyword>,
    /// When the message arrived at the source (IMAP `INTERNALDATE`, JMAP `receivedAt`), or
    /// `None` to let the target stamp the time of the import.
    pub received_at: Option<UtcDateTime>,
}

impl MessageMetadata {
    /// Carries `keywords` (builder-style).
    #[must_use]
    pub fn with_keywords(mut self, keywords: BTreeSet<Keyword>) -> Self {
        self.keywords = keywords;
        self
    }

    /// Carries `at` as the time the message arrived (builder-style).
    #[must_use]
    pub fn with_received_at(mut self, at: UtcDateTime) -> Self {
        self.received_at = Some(at);
        self
    }
}
