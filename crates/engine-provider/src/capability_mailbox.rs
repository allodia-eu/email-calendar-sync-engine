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

    /// Marks giving a new folder a role as it is created as supported
    /// ([`MailboxEdit::create_with_role`](crate::MailboxEdit::create_with_role)): JMAP's
    /// `Mailbox/set` `role`, and IMAP's `CREATE … (USE (…))` where the server advertises
    /// `CREATE-SPECIAL-USE` (RFC 6154 §3) and roles can be listed back. Graph and Gmail cannot:
    /// their role folders exist already, and no request makes another.
    #[must_use]
    pub const fn with_mailbox_create_role(mut self) -> Self {
        self.mailbox_create_role = true;
        self
    }

    /// Whether a new folder can be given a role as it is created.
    ///
    /// `false` means a create that asks for one is refused before anything is sent
    /// ([`CreateRoleError::NotSupported`](crate::CreateRoleError::NotSupported)), so a host
    /// that mirrors folders creates the folder without the role, or not at all. `true` is not
    /// every role: one the transport has no form for is still refused
    /// ([`CreateRoleError::NoTransportForm`](crate::CreateRoleError::NoTransportForm): the engine's
    /// own [`ServerRecords`](engine_core::mail::MailboxRole::ServerRecords) and
    /// [`ChatHistory`](engine_core::mail::MailboxRole::ChatHistory), any
    /// [`Other`](engine_core::mail::MailboxRole::Other), and on IMAP the inbox), and the server
    /// may refuse a role another folder holds. Never `true` without
    /// [`mailbox_writes`](Self::mailbox_writes).
    #[must_use]
    pub const fn mailbox_create_role(self) -> bool {
        self.mailbox_writes && self.mailbox_create_role
    }
}
