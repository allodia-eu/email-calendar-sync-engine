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
    /// An IMAP per-account mailbox-list (folder discovery) scope.
    ///
    /// IMAP carries no sync state for the folder list itself — a `LIST`
    /// re-discovers it as a snapshot each pass — but it is a distinct **container**
    /// scope, claimed and applied before the per-[`ImapMailbox`](Self::ImapMailbox)
    /// email it parents (`store-and-sync.md` referential apply order). Distinct from
    /// any single mailbox's email scope so the two never share a lease.
    ImapMailboxList {
        /// The account.
        account: AccountId,
    },
    /// An IMAP `(account, mailbox)` scope.
    ImapMailbox {
        /// The account.
        account: AccountId,
        /// The mailbox.
        mailbox: MailboxId,
    },
    /// A CalDAV/CardDAV per-account collection-list (calendar/address-book
    /// discovery) scope.
    ///
    /// Like [`ImapMailboxList`](Self::ImapMailboxList), the collection list is
    /// re-discovered as a snapshot each pass (a `PROPFIND` of the
    /// calendar/address-book home), so it carries no cursor of its own — but it is
    /// a distinct **container** scope, claimed and applied before the per-collection
    /// [`DavCollection`](Self::DavCollection) members it parents
    /// (`store-and-sync.md` referential apply order). Distinct from any single
    /// collection's scope so the two never share a lease.
    DavCollectionList {
        /// The account.
        account: AccountId,
    },
    /// A CalDAV/CardDAV `(account, collection)` scope.
    DavCollection {
        /// The account.
        account: AccountId,
        /// The WebDAV collection.
        collection: DavCollectionId,
    },
    /// A Microsoft Graph per-account mail-folder-list (folder discovery) scope.
    ///
    /// Like [`ImapMailboxList`](Self::ImapMailboxList), the folder list is
    /// re-discovered as a snapshot each pass (`GET /me/mailFolders`), so it carries
    /// no cursor of its own — but it is a distinct **container** scope, claimed and
    /// applied before the per-folder [`GraphFolder`](Self::GraphFolder) message
    /// scopes it parents (`store-and-sync.md` referential apply order). Distinct
    /// from any single folder's scope so the two never share a lease.
    GraphFolderList {
        /// The account.
        account: AccountId,
    },
    /// A Microsoft Graph `(account, mail folder)` message scope.
    ///
    /// Graph mail `delta` is rooted at a folder
    /// (`/me/mailFolders/{id}/messages/delta`) with a per-folder `deltaLink` cursor
    /// — there is no account-wide message delta — so message sync is per folder,
    /// like [`ImapMailbox`](Self::ImapMailbox) (but keyed by stable account-global
    /// immutable ids, not per-folder UIDs). A Graph provider is bound to one folder
    /// for email; the cross-folder fan-out is the orchestrator's job.
    GraphFolder {
        /// The account.
        account: AccountId,
        /// The mail folder.
        folder: MailboxId,
    },
}

impl SyncScope {
    /// Returns the account this scope belongs to.
    #[must_use]
    pub fn account(&self) -> &AccountId {
        match self {
            Self::JmapType { account, .. }
            | Self::ImapMailboxList { account }
            | Self::ImapMailbox { account, .. }
            | Self::DavCollectionList { account }
            | Self::DavCollection { account, .. }
            | Self::GraphFolderList { account }
            | Self::GraphFolder { account, .. } => account,
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

    #[test]
    fn imap_mailbox_list_is_distinct_from_a_mailbox_and_roundtrips() {
        // The folder-list container scope must never collide with the email scope
        // of any single mailbox, or the two would share one lease.
        let list = SyncScope::ImapMailboxList { account: account() };
        let inbox = SyncScope::ImapMailbox {
            account: account(),
            mailbox: MailboxId::try_from("INBOX").unwrap(),
        };
        assert_ne!(list, inbox);
        assert_eq!(list.account(), &account());
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(serde_json::from_str::<SyncScope>(&json).unwrap(), list);
    }

    #[test]
    fn graph_folder_list_is_distinct_from_a_folder_and_roundtrips() {
        // The folder-list container scope must never collide with the message
        // scope of any single folder, or the two would share one lease. Graph mail
        // delta is per-folder (no account-wide message delta), so each folder is a
        // distinct member scope.
        let list = SyncScope::GraphFolderList { account: account() };
        let inbox = SyncScope::GraphFolder {
            account: account(),
            folder: MailboxId::try_from("folder-inbox").unwrap(),
        };
        assert_ne!(list, inbox);
        assert_eq!(list.account(), &account());
        assert_eq!(inbox.account(), &account());
        for scope in [&list, &inbox] {
            let json = serde_json::to_string(scope).unwrap();
            assert_eq!(&serde_json::from_str::<SyncScope>(&json).unwrap(), scope);
        }
    }

    #[test]
    fn dav_collection_list_is_distinct_from_a_collection_and_roundtrips() {
        // The calendar/address-book-list container scope must never collide with
        // the events/contacts scope of any single collection, or the two would
        // share one lease.
        let list = SyncScope::DavCollectionList { account: account() };
        let calendar = SyncScope::DavCollection {
            account: account(),
            collection: DavCollectionId::try_from("/dav/cal/alice/default/").unwrap(),
        };
        assert_ne!(list, calendar);
        assert_eq!(list.account(), &account());
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(serde_json::from_str::<SyncScope>(&json).unwrap(), list);
    }
}
