//! Outbound mail-mutation shapes (mark-read/flag, move, delete).
//!
//! These mirror the calendar [`EventWrite`](crate::EventWrite)/
//! [`EventDeletion`](crate::EventDeletion) pair: a serializable request a caller
//! stores as a durable outbox `PendingOp` payload before the side effect, plus a
//! receipt the outbox records on success.
//!
//! A mutation targets one already-synced [`Message`](engine_core::mail::Message) by
//! its provider key and changes one of its three independent axes (`modeling.md`):
//! its [`Keyword`]s (the user-settable read/flagged state), its mailbox membership
//! (collection placement — the mechanism behind a move and a Trash "delete"), or its
//! existence (a permanent delete). The shape is provider-neutral — JMAP expresses all
//! three as one `Email/set` (a `keywords/*`/`mailboxIds/*` patch or a `destroy`),
//! while IMAP maps them to `UID STORE`, `UID MOVE`, and `UID EXPUNGE` respectively.

use std::collections::BTreeSet;

use engine_core::ids::{MailboxId, ProviderKey};
use engine_core::mail::{Keyword, SystemKeyword};
use serde::{Deserialize, Serialize};

/// A request to mutate one already-synced mail object.
///
/// Every variant names its `target` message by provider key; the outbox serializes
/// ops sharing a target so two edits of one message never race. Serializable so it
/// can be stored as a durable outbox payload before the side effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailEdit {
    /// Add and/or remove keywords on a message — mark read/unread (`$seen`),
    /// flag/unflag (`$flagged`), and any other keyword. IMAP `UID STORE ±FLAGS`;
    /// JMAP `Email/set` `keywords/*` patch. `add` and `remove` must be disjoint; an
    /// empty set on either side is a no-op for that direction.
    SetKeywords {
        /// The message to edit.
        target: ProviderKey,
        /// Keywords to set.
        add: BTreeSet<Keyword>,
        /// Keywords to clear.
        remove: BTreeSet<Keyword>,
    },
    /// Move a message to another mailbox — a folder change, and the mechanism behind
    /// "move to Trash" (the caller resolves the Trash mailbox). IMAP `UID MOVE`; JMAP
    /// `Email/set` `mailboxIds` patch.
    MoveTo {
        /// The message to move.
        target: ProviderKey,
        /// The destination mailbox.
        destination: MailboxId,
    },
    /// **Permanently** delete a message — irreversible, not a Trash move. IMAP
    /// `UID STORE +FLAGS (\Deleted)` then `UID EXPUNGE`; JMAP `Email/set` `destroy`.
    Delete {
        /// The message to delete.
        target: ProviderKey,
    },
}

impl MailEdit {
    /// Marks a message read (`add $seen`) or unread (`remove $seen`).
    #[must_use]
    pub fn mark_seen(target: ProviderKey, seen: bool) -> Self {
        Self::toggle_keyword(target, SystemKeyword::Seen, seen)
    }

    /// Flags (`add $flagged`) or unflags (`remove $flagged`) a message.
    #[must_use]
    pub fn set_flagged(target: ProviderKey, flagged: bool) -> Self {
        Self::toggle_keyword(target, SystemKeyword::Flagged, flagged)
    }

    /// Adds the keyword when `set`, else removes it.
    fn toggle_keyword(target: ProviderKey, keyword: SystemKeyword, set: bool) -> Self {
        let keyword = Keyword::system(keyword);
        let (add, remove) = if set {
            (BTreeSet::from([keyword]), BTreeSet::new())
        } else {
            (BTreeSet::new(), BTreeSet::from([keyword]))
        };
        Self::SetKeywords {
            target,
            add,
            remove,
        }
    }

    /// Moves a message to `destination`.
    #[must_use]
    pub fn move_to(target: ProviderKey, destination: MailboxId) -> Self {
        Self::MoveTo {
            target,
            destination,
        }
    }

    /// Permanently deletes a message.
    #[must_use]
    pub fn delete(target: ProviderKey) -> Self {
        Self::Delete { target }
    }

    /// The message this edit targets — the resource it serializes on in the outbox.
    #[must_use]
    pub fn target(&self) -> &ProviderKey {
        match self {
            Self::SetKeywords { target, .. }
            | Self::MoveTo { target, .. }
            | Self::Delete { target } => target,
        }
    }
}

/// The result of a successful [`MailEdit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEditReceipt {
    /// The provider key the outbox records as resolved. The edited message's key —
    /// for a move that is the source key (the destination copy reconciles on the
    /// next sync of that mailbox, since IMAP move synthesizes a new key there), and
    /// for a delete it is the now-removed key (the next snapshot tombstones it).
    pub message_key: ProviderKey,
}

impl MailEditReceipt {
    /// Records a successful edit.
    #[must_use]
    pub fn new(message_key: ProviderKey) -> Self {
        Self { message_key }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ProviderKey {
        ProviderKey::new("imap:v1:u42@INBOX").unwrap()
    }

    #[test]
    fn mark_seen_toggles_the_seen_keyword() {
        let read = MailEdit::mark_seen(target(), true);
        let MailEdit::SetKeywords { add, remove, .. } = &read else {
            panic!("expected SetKeywords");
        };
        assert!(add.contains(&Keyword::system(SystemKeyword::Seen)));
        assert!(remove.is_empty());

        let unread = MailEdit::mark_seen(target(), false);
        let MailEdit::SetKeywords { add, remove, .. } = &unread else {
            panic!("expected SetKeywords");
        };
        assert!(add.is_empty());
        assert!(remove.contains(&Keyword::system(SystemKeyword::Seen)));
    }

    #[test]
    fn set_flagged_toggles_the_flagged_keyword() {
        let flagged = MailEdit::set_flagged(target(), true);
        let MailEdit::SetKeywords { add, .. } = &flagged else {
            panic!("expected SetKeywords");
        };
        assert!(add.contains(&Keyword::system(SystemKeyword::Flagged)));
    }

    #[test]
    fn target_is_the_serialized_resource_for_every_variant() {
        let dest = MailboxId::try_from("Archive").unwrap();
        let edits = [
            MailEdit::mark_seen(target(), true),
            MailEdit::move_to(target(), dest),
            MailEdit::delete(target()),
        ];
        for edit in &edits {
            assert_eq!(edit.target(), &target());
        }
    }

    #[test]
    fn receipt_carries_the_resolved_key() {
        let receipt = MailEditReceipt::new(target());
        assert_eq!(receipt.message_key, target());
    }
}
