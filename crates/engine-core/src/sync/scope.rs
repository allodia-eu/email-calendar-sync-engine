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

/// The search domain whose member objects a scope holds — the index a per-account
/// query routes the scope to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchDomain {
    /// Mail objects (the mail scalar/address/membership index plus full text).
    Mail,
    /// Calendar events (the event scalar/participant index, occurrences, full text).
    Calendar,
}

/// The kind of member object a scope holds, so a host can read an account's objects
/// (mailboxes, messages, calendars, events) by kind without branching on protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// A mail collection (mailbox/folder/label).
    Mailbox,
    /// A mail object (message).
    Message,
    /// A calendar collection.
    Calendar,
    /// A calendar event.
    Event,
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

    /// The kind of member object this scope holds, or `None` for a scope whose objects
    /// are not host-facing view objects (a JMAP `Thread` or `EmailSubmission`).
    ///
    /// This is how a host reads an account's objects without hard-coding or branching
    /// on protocol: enumerate the account's scopes (`StoreRead::account_scopes`), then
    /// read the ones whose kind it wants. CalDAV collections classify as calendar
    /// today; CardDAV address books will need disambiguation when contacts land (they
    /// reuse [`DavCollection`](Self::DavCollection) /
    /// [`DavCollectionList`](Self::DavCollectionList)).
    #[must_use]
    pub fn object_kind(&self) -> Option<ObjectKind> {
        match self {
            Self::JmapType { data_type, .. } => match data_type {
                JmapDataType::Email => Some(ObjectKind::Message),
                JmapDataType::Mailbox => Some(ObjectKind::Mailbox),
                JmapDataType::CalendarEvent => Some(ObjectKind::Event),
                JmapDataType::Calendar => Some(ObjectKind::Calendar),
                _ => None,
            },
            Self::ImapMailbox { .. } => Some(ObjectKind::Message),
            Self::ImapMailboxList { .. } => Some(ObjectKind::Mailbox),
            Self::DavCollection { .. } => Some(ObjectKind::Event),
            Self::DavCollectionList { .. } => Some(ObjectKind::Calendar),
        }
    }

    /// The search domain whose member objects this scope holds, or `None` for a scope
    /// whose objects are not directly searchable (a mailbox/calendar collection or
    /// discovery scope, or a JMAP `Thread`/`EmailSubmission`). Derived from
    /// [`object_kind`](Self::object_kind): only message and event scopes are searchable.
    ///
    /// A per-account search enumerates the account's scopes and routes each through the
    /// matching index by this, so callers never hard-code which scopes a provider uses.
    #[must_use]
    pub fn search_domain(&self) -> Option<SearchDomain> {
        match self.object_kind() {
            Some(ObjectKind::Message) => Some(SearchDomain::Mail),
            Some(ObjectKind::Event) => Some(SearchDomain::Calendar),
            Some(ObjectKind::Mailbox | ObjectKind::Calendar) | None => None,
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
    fn search_domain_routes_objects_and_skips_containers() {
        use SearchDomain::{Calendar, Mail};
        let a = account();
        // Mail-object scopes.
        let jmap_mail = SyncScope::JmapType {
            account: a.clone(),
            data_type: JmapDataType::Email,
        };
        let imap = SyncScope::ImapMailbox {
            account: a.clone(),
            mailbox: MailboxId::try_from("INBOX").unwrap(),
        };
        assert_eq!(jmap_mail.search_domain(), Some(Mail));
        assert_eq!(imap.search_domain(), Some(Mail));
        // Calendar-object scopes.
        let jmap_cal = SyncScope::JmapType {
            account: a.clone(),
            data_type: JmapDataType::CalendarEvent,
        };
        let dav = SyncScope::DavCollection {
            account: a.clone(),
            collection: DavCollectionId::try_from("/dav/cal/a/default/").unwrap(),
        };
        assert_eq!(jmap_cal.search_domain(), Some(Calendar));
        assert_eq!(dav.search_domain(), Some(Calendar));
        // Containers and discovery scopes hold no directly searchable objects.
        for data_type in [
            JmapDataType::Mailbox,
            JmapDataType::Calendar,
            JmapDataType::Thread,
            JmapDataType::EmailSubmission,
        ] {
            let container = SyncScope::JmapType {
                account: a.clone(),
                data_type,
            };
            assert_eq!(container.search_domain(), None, "{container:?}");
        }
        assert_eq!(
            SyncScope::ImapMailboxList { account: a.clone() }.search_domain(),
            None
        );
        assert_eq!(
            SyncScope::DavCollectionList { account: a }.search_domain(),
            None
        );
    }

    #[test]
    fn object_kind_classifies_every_scope() {
        use ObjectKind::{Calendar, Event, Mailbox, Message};
        let a = account();
        let jmap = |data_type| SyncScope::JmapType {
            account: a.clone(),
            data_type,
        };
        assert_eq!(jmap(JmapDataType::Email).object_kind(), Some(Message));
        assert_eq!(jmap(JmapDataType::Mailbox).object_kind(), Some(Mailbox));
        assert_eq!(jmap(JmapDataType::CalendarEvent).object_kind(), Some(Event));
        assert_eq!(jmap(JmapDataType::Calendar).object_kind(), Some(Calendar));
        // JMAP types with no host-facing view object.
        assert_eq!(jmap(JmapDataType::Thread).object_kind(), None);
        assert_eq!(jmap(JmapDataType::EmailSubmission).object_kind(), None);
        // IMAP / CalDAV scopes.
        assert_eq!(
            SyncScope::ImapMailbox {
                account: a.clone(),
                mailbox: MailboxId::try_from("INBOX").unwrap(),
            }
            .object_kind(),
            Some(Message)
        );
        assert_eq!(
            SyncScope::ImapMailboxList { account: a.clone() }.object_kind(),
            Some(Mailbox)
        );
        assert_eq!(
            SyncScope::DavCollection {
                account: a.clone(),
                collection: DavCollectionId::try_from("/dav/cal/a/default/").unwrap(),
            }
            .object_kind(),
            Some(Event)
        );
        assert_eq!(
            SyncScope::DavCollectionList { account: a }.object_kind(),
            Some(Calendar)
        );
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
