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
//!
//! They disagree on names too, so each adapter refuses exactly the names its own transport
//! cannot carry, before it sends anything, and nothing more. The engine's portable name policy
//! ([`validate_mailbox_name`](engine_core::mail::validate_mailbox_name)) is a host's to apply
//! where it asks a user for a name; the outbox applies it before it queues a change.
//!
//! And on roles. A create may ask for one ([`MailboxEdit::create_with_role`]), which only some
//! targets can give a folder as they make it
//! ([`Capabilities::mailbox_create_role`]):
//!
//! | | a role on create | a role it has no form for |
//! |---|---|---|
//! | JMAP | `Mailbox/set` `role` (RFC 8621 §2), one folder per role | not a registered role: [`MailboxRole::jmap_role`] |
//! | IMAP | `CREATE … (USE (…))` where the server advertises `CREATE-SPECIAL-USE` (RFC 6154 §3) and roles can be listed back | the inbox, and any role without an attribute: [`MailboxRole::as_imap_special_use`] |
//! | Graph | none: its well-known folders exist already | — |
//! | Gmail | none: its system labels exist already | — |
//!
//! Every adapter refuses a role it cannot set before it sends anything, by one rule
//! ([`MailboxEdit::role_as`]), gated on its own capabilities; and after a create the server
//! accepted, it reads the role back ([`CreateRoleError::NotKept`]). A folder made without the
//! role it was asked for would leave a host believing the target has one.

use core::fmt;

use engine_core::{
    ids::MailboxId,
    mail::{MailboxNameError, MailboxRole},
};
use serde::{Deserialize, Serialize};

use crate::{Capabilities, ProviderError};

/// A change to one folder of an account's tree, as an adapter performs it.
///
/// `parent: None` means the top of the account's tree, never "unchanged": every variant
/// that places a folder states where, so a queued edit replayed later lands exactly where
/// the user put it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxEdit {
    /// Creates a folder called `name` inside `parent`, with `role` where one is asked for.
    ///
    /// Built by [`MailboxEdit::create`] or [`MailboxEdit::create_with_role`]: the variant is
    /// `#[non_exhaustive]`, so a field it gains later breaks no caller. A match names the
    /// fields it reads and ends with `..`.
    #[non_exhaustive]
    Create {
        /// The new folder's own name: one level, never a path.
        name: String,
        /// The folder to create it in, or `None` for the top level.
        parent: Option<MailboxId>,
        /// The role the new folder is to have, or `None` for an ordinary folder.
        ///
        /// Only a target with
        /// [`Capabilities::mailbox_create_role`](crate::Capabilities::mailbox_create_role) can
        /// give one, and only a role its transport has a form for ([`MailboxEdit::role_as`]);
        /// any other is refused before anything is sent. Absent from the serialized form when
        /// `None`, so an edit queued before roles existed reads back unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<MailboxRole>,
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
    /// A create of an ordinary folder called `name` inside `parent` (`None` for the top level).
    #[must_use]
    pub fn create(name: impl Into<String>, parent: Option<MailboxId>) -> Self {
        Self::Create {
            name: name.into(),
            parent,
            role: None,
        }
    }

    /// A create of a folder called `name` inside `parent` that is to have `role`: a Sent folder,
    /// say, on a target that has none.
    ///
    /// A constructor of its own rather than a `with_role` on every edit, which would have to
    /// drop the role from an update or panic: neither is something a caller should discover.
    #[must_use]
    pub fn create_with_role(
        name: impl Into<String>,
        parent: Option<MailboxId>,
        role: MailboxRole,
    ) -> Self {
        Self::Create {
            name: name.into(),
            parent,
            role: Some(role),
        }
    }

    /// The role a create asks for, or `None` for a create of an ordinary folder and for every
    /// other edit.
    #[must_use]
    pub fn requested_role(&self) -> Option<&MailboxRole> {
        match self {
            Self::Create { role, .. } => role.as_ref(),
            Self::Update { .. } | Self::Trash { .. } | Self::Delete { .. } => None,
        }
    }

    /// The role a create asks for, in the form the target's transport sends it: the one rule
    /// every adapter applies before it sends a folder edit.
    ///
    /// `capabilities` are the adapter's own, so the gate is the one a host reads
    /// ([`Capabilities::mailbox_create_role`]); `form` is the transport's name for a role, such
    /// as [`MailboxRole::jmap_role`] or [`MailboxRole::as_imap_special_use`]. `Ok(None)` when no
    /// role is asked for.
    ///
    /// # Errors
    ///
    /// [`CreateRoleError::NotSupported`] for a role asked of a target whose capabilities say it
    /// sets none, and [`CreateRoleError::NoTransportForm`] for a role `form` has no name for.
    /// Either converts to an [`InvalidState`](engine_core::error::FailureClass::InvalidState)
    /// [`ProviderError`].
    pub fn role_as(
        &self,
        capabilities: &Capabilities,
        form: impl FnOnce(&MailboxRole) -> Option<&'static str>,
    ) -> Result<Option<&'static str>, CreateRoleError> {
        let Some(role) = self.requested_role() else {
            return Ok(None);
        };
        if !capabilities.mailbox_create_role() {
            return Err(CreateRoleError::NotSupported);
        }
        form(role).map(Some).ok_or(CreateRoleError::NoTransportForm)
    }

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

/// A name outside the portable policy is
/// [`InvalidState`](engine_core::error::FailureClass::InvalidState), the class an adapter
/// gives a name its transport cannot carry: the change cannot succeed as asked, however
/// often it is retried. The detail is the rule broken, never the name, and the typed error
/// rides along as the [`source`](std::error::Error::source), so a host can tell which rule.
impl From<MailboxNameError> for ProviderError {
    fn from(error: MailboxNameError) -> Self {
        ProviderError::invalid_state(error.to_string()).with_source(error)
    }
}

/// Why a create that asked for a role did not make a folder with it.
///
/// Two are refusals before anything is sent ([`MailboxEdit::role_as`]) and change nothing.
/// [`NotKept`](Self::NotKept) is what the server did with a create it accepted, and is **the one
/// with a side effect**: the folder exists, without the role, under the id it carries. The
/// message says which, never the role's name, which may be a server's own spelling
/// ([`MailboxRole::Other`]), nor the folder, whose id may be its name (IMAP).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CreateRoleError {
    /// The target cannot give a folder a role as it makes it
    /// ([`Capabilities::mailbox_create_role`] is `false`): Graph's well-known folders and
    /// Gmail's system labels exist already and no request makes another; a JMAP account that
    /// cannot change its folders; an IMAP server that does not advertise `CREATE-SPECIAL-USE`
    /// (RFC 6154 §3) or cannot list roles at all. Nothing was sent.
    NotSupported,
    /// The role has no form on the target's transport: no registry names
    /// [`ServerRecords`](MailboxRole::ServerRecords) or [`ChatHistory`](MailboxRole::ChatHistory),
    /// an [`Other`](MailboxRole::Other) is a role some server made up, and IMAP names the inbox
    /// by its name rather than an attribute. Nothing was sent.
    NoTransportForm,
    /// The server accepted the create and the folder it made does not have the role, read back
    /// right after. **This refusal has a side effect**: the folder exists, as an ordinary one,
    /// finished as any create finishes it (subscribed, on IMAP), under `mailbox`. The adapter
    /// never deletes it. A host records it as made, or deletes it if it does not want it; a
    /// retry of the same create meets it by name and is a
    /// [`Conflict`](engine_core::error::FailureClass::Conflict).
    NotKept {
        /// The folder the create made, without the role.
        mailbox: MailboxId,
    },
}

impl fmt::Display for CreateRoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotSupported => "this account cannot give a folder a role as it creates it",
            Self::NoTransportForm => "this account has no way to name that role",
            Self::NotKept { .. } => "the server made the folder without the role",
        })
    }
}

impl std::error::Error for CreateRoleError {}

/// [`InvalidState`](engine_core::error::FailureClass::InvalidState), as a refused name is: the
/// create cannot succeed as asked, however often it is retried. The typed error rides along as
/// the [`source`](std::error::Error::source), so a host can tell which.
impl From<CreateRoleError> for ProviderError {
    fn from(error: CreateRoleError) -> Self {
        ProviderError::invalid_state(error.to_string()).with_source(error)
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
#[path = "mailbox_edit_tests.rs"]
mod tests;
