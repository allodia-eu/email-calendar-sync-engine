//! Outbound changes to an account's folder tree: create, rename or move, trash, delete.
//!
//! A [`MailboxEdit`] is what an adapter is asked to do, already resolved: the name a
//! trashed folder takes inside Trash has been chosen, and the caller has checked that the
//! folder is still where the user last saw it. The queued intent a host enqueues, and the
//! check that runs before this is sent, live in `engine-sync`.
//!
//! The four transports disagree on almost everything underneath:
//!
//! | | a folder is named by | a rename or move | subfolders |
//! |---|---|---|---|
//! | IMAP | its full path | `RENAME`, which **moves the key** | renamed with it (RFC 9051 §6.3.6) |
//! | JMAP | a server id | `Mailbox/set` `name`/`parentId` | follow by id |
//! | Graph | an immutable id | `PATCH displayName`, `POST …/move` | follow by id |
//! | Gmail | a label id | the label's full name, `/`-separated | each is a label of its own |
//!
//! That is why a receipt names the folder the edit **resolved to**: on IMAP a rename or a
//! trash hands back a different key, and a host that kept the old one would name a folder
//! the server no longer has.

use engine_core::ids::MailboxId;
use serde::{Deserialize, Serialize};

/// A change to one folder of an account's tree, as an adapter performs it.
///
/// `parent: None` means the top of the account's tree, never "unchanged": every variant
/// that places a folder states where, so a queued edit replayed later lands exactly where
/// the user put it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxEdit {
    /// Creates a folder called `name` inside `parent`.
    Create {
        /// The new folder's own name: one level, never a path.
        name: String,
        /// The folder to create it in, or `None` for the top level.
        parent: Option<MailboxId>,
    },
    /// Renames a folder, moves it, or both. Its subfolders go with it.
    Update {
        /// The folder to change.
        target: MailboxId,
        /// The name it is to have.
        name: String,
        /// The folder it is to be inside, or `None` for the top level.
        parent: Option<MailboxId>,
    },
    /// Puts a folder, its subfolders and their mail in Trash.
    ///
    /// Where the transport can file a folder inside Trash (IMAP, JMAP, Graph) the folder
    /// is moved there whole, under `name`, and can be moved back out. Gmail cannot nest a
    /// label under its Trash, so there each message that is filed nowhere else goes to
    /// Trash and the labels are removed; a message also filed elsewhere keeps that filing.
    Trash {
        /// The folder to trash.
        target: MailboxId,
        /// The account's Trash folder.
        trash: MailboxId,
        /// The name it takes inside Trash, chosen so it collides with nothing there.
        name: String,
    },
    /// **Permanently** removes a folder, its subfolders and their mail.
    ///
    /// Irreversible, so a host offers it only for a folder already in Trash. A folder
    /// that is already gone is a success: the op is retried, and a retry meets the
    /// folder the first attempt removed.
    Delete {
        /// The folder to remove.
        target: MailboxId,
    },
}

impl MailboxEdit {
    /// The folder the edit changes, or `None` for a create, which changes nothing yet.
    #[must_use]
    pub fn target(&self) -> Option<&MailboxId> {
        match self {
            Self::Create { .. } => None,
            Self::Update { target, .. } | Self::Trash { target, .. } | Self::Delete { target } => {
                Some(target)
            }
        }
    }
}

/// The result of a successful [`MailboxEdit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxEditReceipt {
    /// The folder the edit resolved to: the new folder for a create, the folder's key
    /// **after** a rename, move or trash (which on IMAP is not the key it had), and `None`
    /// when no folder remains (a delete, or a trash on a transport that removes the folder).
    pub mailbox: Option<MailboxId>,
}

impl MailboxEditReceipt {
    /// A receipt naming the folder the edit resolved to.
    #[must_use]
    pub fn resolved(mailbox: MailboxId) -> Self {
        Self {
            mailbox: Some(mailbox),
        }
    }

    /// A receipt for an edit that left no folder behind.
    #[must_use]
    pub fn removed() -> Self {
        Self { mailbox: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> MailboxId {
        MailboxId::try_from(value).unwrap()
    }

    #[test]
    fn a_create_targets_nothing_and_every_other_edit_its_folder() {
        let create = MailboxEdit::Create {
            name: "Receipts".into(),
            parent: None,
        };
        assert_eq!(create.target(), None);
        let edits = [
            MailboxEdit::Update {
                target: id("f1"),
                name: "Bills".into(),
                parent: Some(id("f0")),
            },
            MailboxEdit::Trash {
                target: id("f1"),
                trash: id("trash"),
                name: "Bills".into(),
            },
            MailboxEdit::Delete { target: id("f1") },
        ];
        for edit in &edits {
            assert_eq!(edit.target(), Some(&id("f1")));
        }
    }

    #[test]
    fn an_edit_survives_the_outbox_round_trip() {
        let edit = MailboxEdit::Update {
            target: id("Work/2024"),
            name: "2025".into(),
            parent: None,
        };
        let json = serde_json::to_value(&edit).unwrap();
        assert_eq!(serde_json::from_value::<MailboxEdit>(json).unwrap(), edit);
    }

    #[test]
    fn a_receipt_names_the_folder_or_says_none_remains() {
        assert_eq!(
            MailboxEditReceipt::resolved(id("Trash/Bills")).mailbox,
            Some(id("Trash/Bills"))
        );
        assert_eq!(MailboxEditReceipt::removed().mailbox, None);
    }
}
