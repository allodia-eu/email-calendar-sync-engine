//! Sync scopes.

use serde::{Deserialize, Serialize};

use crate::ids::{AccountId, DavCollectionId, MailboxId};

open_enum! {
    /// A JMAP data type, whose `/changes` state is tracked per account
    /// (RFC 8620 §1.6.3, §5.2). Wire names match the JMAP type names.
    JmapDataType {
        /// `Email` objects.
        Email => "Email",
        /// `Mailbox` collections.
        Mailbox => "Mailbox",
        /// `Thread` objects.
        Thread => "Thread",
        /// `EmailSubmission` objects.
        EmailSubmission => "EmailSubmission",
        /// `Calendar` collections.
        Calendar => "Calendar",
        /// `CalendarEvent` objects.
        CalendarEvent => "CalendarEvent",
    }
}

impl JmapDataType {
    /// Returns `true` if this type is a *container* (collections), which must be
    /// applied before the *member* types that reference it (store-and-sync.md
    /// referential apply order).
    #[must_use]
    pub fn is_container(&self) -> bool {
        matches!(self, Self::Mailbox | Self::Calendar)
    }
}

/// The unit of sync state, leasing, and serialization.
///
/// Granularity is dictated by the protocol, and the three disagree
/// (`store-and-sync.md`), so this is an enum, not a single id:
///
/// - **JMAP** state is per account, per data type.
/// - **IMAP** state is per mailbox (`UIDVALIDITY`/`UIDNEXT`/`HIGHESTMODSEQ`).
/// - **CalDAV/CardDAV** state is per collection (sync-token, or CTag + ETags).
///
/// SMTP is not a sync scope; it is an outbox transport leased per account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SyncScope {
    /// A JMAP `(account, data type)` scope.
    JmapType {
        /// The account.
        account: AccountId,
        /// The JMAP data type.
        data_type: JmapDataType,
    },
    /// An IMAP `(account, mailbox)` scope.
    ImapMailbox {
        /// The account.
        account: AccountId,
        /// The mailbox.
        mailbox: MailboxId,
    },
    /// A CalDAV/CardDAV `(account, collection)` scope.
    DavCollection {
        /// The account.
        account: AccountId,
        /// The WebDAV collection.
        collection: DavCollectionId,
    },
}

impl SyncScope {
    /// Returns the account this scope belongs to.
    #[must_use]
    pub fn account(&self) -> &AccountId {
        match self {
            Self::JmapType { account, .. }
            | Self::ImapMailbox { account, .. }
            | Self::DavCollection { account, .. } => account,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountId {
        AccountId::try_from("acct-1").unwrap()
    }

    #[test]
    fn scope_exposes_account() {
        let scope = SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Email,
        };
        assert_eq!(scope.account(), &account());
    }

    #[test]
    fn container_types_apply_before_members() {
        assert!(JmapDataType::Mailbox.is_container());
        assert!(JmapDataType::Calendar.is_container());
        assert!(!JmapDataType::Email.is_container());
        assert!(!JmapDataType::CalendarEvent.is_container());
    }

    #[test]
    fn scopes_are_distinct_and_hashable() {
        let jmap = SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Email,
        };
        let imap = SyncScope::ImapMailbox {
            account: account(),
            mailbox: MailboxId::try_from("inbox").unwrap(),
        };
        assert_ne!(jmap, imap);
        let json = serde_json::to_string(&jmap).unwrap();
        assert_eq!(serde_json::from_str::<SyncScope>(&json).unwrap(), jmap);
    }
}
