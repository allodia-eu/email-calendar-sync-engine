//! Tests for [`SyncScope`](super::SyncScope) — account/object-kind/search-domain
//! classification and per-variant serde roundtrips.

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
        SyncScope::DavCollectionList { account: a.clone() }.object_kind(),
        Some(Calendar)
    );
    // Graph scopes mirror IMAP: a per-folder message scope + the folder-list
    // container.
    assert_eq!(
        SyncScope::GraphFolder {
            account: a.clone(),
            folder: MailboxId::try_from("folder-inbox").unwrap(),
        }
        .object_kind(),
        Some(Message)
    );
    assert_eq!(
        SyncScope::GraphFolderList { account: a }.object_kind(),
        Some(Mailbox)
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
fn graph_calendar_list_is_distinct_from_a_calendar_and_roundtrips() {
    // The calendar-list container scope must never collide with the event scope of
    // any single calendar, or the two would share one lease. Graph calendar sync is
    // per calendar (time-windowed calendarView/delta), so each calendar is a
    // distinct member scope, mirroring the mail GraphFolder/GraphFolderList split.
    let list = SyncScope::GraphCalendarList { account: account() };
    let calendar = SyncScope::GraphCalendar {
        account: account(),
        calendar: CalendarId::try_from("AAkALgcal-default").unwrap(),
    };
    assert_ne!(list, calendar);
    assert_eq!(list.account(), &account());
    assert_eq!(calendar.account(), &account());
    assert_eq!(list.object_kind(), Some(ObjectKind::Calendar));
    assert_eq!(calendar.object_kind(), Some(ObjectKind::Event));
    assert_eq!(calendar.search_domain(), Some(SearchDomain::Calendar));
    for scope in [&list, &calendar] {
        let json = serde_json::to_string(scope).unwrap();
        assert_eq!(&serde_json::from_str::<SyncScope>(&json).unwrap(), scope);
    }
}

#[test]
fn gmail_message_scope_is_account_global_and_roundtrips() {
    // Gmail's message scope is account-global (historyId is account-wide, JMAP-like),
    // so there is one message scope per account — not a per-label fan-out — plus the
    // label-list container. The two must never share a lease.
    let messages = SyncScope::GmailMessages { account: account() };
    let labels = SyncScope::GmailLabelList { account: account() };
    assert_ne!(messages, labels);
    assert_eq!(messages.object_kind(), Some(ObjectKind::Message));
    assert_eq!(messages.search_domain(), Some(SearchDomain::Mail));
    assert_eq!(labels.object_kind(), Some(ObjectKind::Mailbox));
    assert_eq!(labels.search_domain(), None);
    for scope in [&messages, &labels] {
        assert_eq!(scope.account(), &account());
        let json = serde_json::to_string(scope).unwrap();
        assert_eq!(&serde_json::from_str::<SyncScope>(&json).unwrap(), scope);
    }
}

#[test]
fn google_calendar_list_is_distinct_from_a_calendar_and_roundtrips() {
    // The calendar-list container scope must never collide with the event scope of
    // any single calendar, or the two would share one lease. Google calendar sync is
    // per calendar (a per-calendar nextSyncToken), so each calendar is a distinct
    // member scope, mirroring the Graph GraphCalendar/GraphCalendarList split.
    let list = SyncScope::GoogleCalendarList { account: account() };
    let calendar = SyncScope::GoogleCalendar {
        account: account(),
        calendar: CalendarId::try_from("primary").unwrap(),
    };
    assert_ne!(list, calendar);
    assert_eq!(list.account(), &account());
    assert_eq!(calendar.account(), &account());
    assert_eq!(list.object_kind(), Some(ObjectKind::Calendar));
    assert_eq!(calendar.object_kind(), Some(ObjectKind::Event));
    assert_eq!(calendar.search_domain(), Some(SearchDomain::Calendar));
    for scope in [&list, &calendar] {
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
