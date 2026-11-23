//! What a mail import sends, what it answers, and the limits a target states.

use std::num::{NonZeroU64, NonZeroUsize};

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::MessageMetadata,
    membership::Memberships,
    raw::RawMime,
};

use crate::{ProviderError, ProviderResult};

/// One message to import: its exact bytes, where it goes, and the metadata it carries.
///
/// The bytes are stored as they are: an importer never re-assembles them, so the target holds the
/// message the source held. `Debug` prints the size, never the bytes ([`RawMime`]).
///
/// Built with [`new`](Self::new) and [`with_metadata`](Self::with_metadata), so a part added
/// later does not break a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MessageImport {
    /// The message, byte for byte (RFC 5322).
    pub raw: RawMime,
    /// The mailboxes to file it in. A target that files a message in one mailbox only refuses
    /// more ([`MailImportLimits::max_mailboxes`]).
    pub mailboxes: Memberships<MailboxId>,
    /// What the message carries beside its bytes ([`MessageMetadata`]).
    pub metadata: MessageMetadata,
}

impl MessageImport {
    /// A message to file in `mailboxes`, carrying no metadata: no keyword, and the arrival time
    /// the target stamps.
    #[must_use]
    pub fn new(raw: RawMime, mailboxes: Memberships<MailboxId>) -> Self {
        Self {
            raw,
            mailboxes,
            metadata: MessageMetadata::default(),
        }
    }

    /// Stores the message with `metadata` (builder-style).
    #[must_use]
    pub fn with_metadata(mut self, metadata: MessageMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

/// What importing one message answers.
///
/// Built with [`created`](Self::created) or [`existing`](Self::existing), so a part added later
/// does not break an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportedMessage {
    /// The key the target stores the message under, where its answer names one. IMAP names it
    /// only with `UIDPLUS`; without it a caller finds the message by its fingerprint.
    pub key: Option<ProviderKey>,
    /// Whether the target already held this message and stored nothing new (a JMAP
    /// `alreadyExists`). The message is there either way.
    pub already_there: bool,
    /// The metadata the stored message carries, of what the import asked for.
    ///
    /// Each field holds what the target kept, so whatever the import asked for and this lacks
    /// was **not** stored: a keyword an IMAP folder cannot keep, an arrival time a target
    /// stamped its own way. The message itself is stored either way. Empty for a message
    /// [already there](Self::already_there), which keeps the metadata it had.
    pub kept: MessageMetadata,
}

impl ImportedMessage {
    /// A message the import stored, under `key` where the target named it, carrying no metadata.
    #[must_use]
    pub fn created(key: Option<ProviderKey>) -> Self {
        Self {
            key,
            already_there: false,
            kept: MessageMetadata::default(),
        }
    }

    /// A message the target already held, under `key`.
    #[must_use]
    pub fn existing(key: ProviderKey) -> Self {
        Self {
            key: Some(key),
            already_there: true,
            kept: MessageMetadata::default(),
        }
    }

    /// Records the metadata the stored message carries ([`Self::kept`]).
    #[must_use]
    pub fn with_kept(mut self, kept: MessageMetadata) -> Self {
        self.kept = kept;
        self
    }
}

/// The limits a target states for the messages it imports.
///
/// What a target states is not all it enforces: a message inside these limits can still be
/// refused, and an importer answers that refusal for that message alone. A host reads them to
/// tell an operator before a migration which messages cannot go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailImportLimits {
    max_message_size: Option<NonZeroU64>,
    max_mailboxes: NonZeroUsize,
}

impl MailImportLimits {
    /// A target that files one message in at most `max_mailboxes` mailboxes and states no
    /// largest size.
    #[must_use]
    pub fn new(max_mailboxes: NonZeroUsize) -> Self {
        Self {
            max_message_size: None,
            max_mailboxes,
        }
    }

    /// The target accepts no message larger than `bytes` (builder-style): IMAP `APPENDLIMIT`,
    /// JMAP `maxSizeUpload`.
    #[must_use]
    pub fn with_max_message_size(mut self, bytes: NonZeroU64) -> Self {
        self.max_message_size = Some(bytes);
        self
    }

    /// The largest message the target accepts, in bytes, or `None` where it states none.
    #[must_use]
    pub fn max_message_size(&self) -> Option<NonZeroU64> {
        self.max_message_size
    }

    /// How many mailboxes one message may be filed in: one on IMAP, the session's
    /// `maxMailboxesPerEmail` on JMAP.
    #[must_use]
    pub fn max_mailboxes(&self) -> NonZeroUsize {
        self.max_mailboxes
    }

    /// Checks `message` against these limits.
    ///
    /// # Errors
    ///
    /// A message larger than the target accepts, or filed in more mailboxes than it allows, is
    /// [`ProviderError::item_unavailable`]: the target will never take that message, and the
    /// rest of the batch is unaffected.
    pub fn check(&self, message: &MessageImport) -> ProviderResult<()> {
        let size = u64::try_from(message.raw.len()).unwrap_or(u64::MAX);
        if let Some(max) = self.max_message_size
            && size > max.get()
        {
            return Err(ProviderError::item_unavailable(format!(
                "a {size}-byte message exceeds the target's {max}-byte limit"
            )));
        }
        let mailboxes = message.mailboxes.len();
        if mailboxes > self.max_mailboxes {
            return Err(ProviderError::item_unavailable(format!(
                "a message filed in {mailboxes} mailboxes exceeds the target's {} per message",
                self.max_mailboxes
            )));
        }
        Ok(())
    }
}
