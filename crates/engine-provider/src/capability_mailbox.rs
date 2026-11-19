//! The folder-tree half of [`Capabilities`]: whether an adapter can change an account's
//! folders, beside reading them.

use crate::Capabilities;

impl Capabilities {
    /// Marks changing the account's folder tree as supported: creating, renaming, moving,
    /// trashing and deleting a folder
    /// ([`MailboxWrites::edit_mailbox`](crate::MailboxWrites::edit_mailbox)).
    ///
    /// Distinct from [`with_mail_writes`](Self::with_mail_writes), which changes a message:
    /// an account may be allowed to file its mail and not to rearrange its folders.
    #[must_use]
    pub const fn with_mailbox_writes(mut self) -> Self {
        self.mailbox_writes = true;
        self
    }

    /// Whether the account's folder tree can be changed.
    #[must_use]
    pub const fn mailbox_writes(self) -> bool {
        self.mailbox_writes
    }
}
