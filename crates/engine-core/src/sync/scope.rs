//! Sync scopes.

use serde::{Deserialize, Serialize};

use crate::ids::{AccountId, CalendarId, DavCollectionId, MailboxId};

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
    /// A Microsoft Graph per-account calendar-list (calendar discovery) scope.
    ///
    /// Like [`GraphFolderList`](Self::GraphFolderList), the calendar list is
    /// re-discovered as a snapshot each pass (`GET /me/calendars`), so it carries no
    /// cursor of its own — but it is a distinct **container** scope, claimed and
    /// applied before the per-calendar [`GraphCalendar`](Self::GraphCalendar) event
    /// scopes it parents (`store-and-sync.md` referential apply order).
    GraphCalendarList {
        /// The account.
        account: AccountId,
    },
    /// A Microsoft Graph `(account, calendar)` event scope.
    ///
    /// Graph calendar sync is per calendar and inherently **time-windowed**
    /// (`/me/calendars/{id}/calendarView/delta` requires a start/end date range, and
    /// the returned `deltaLink` encodes it), so — like [`GraphFolder`](Self::GraphFolder)
    /// for mail — a Graph calendar provider is bound to one calendar for events; the
    /// cross-calendar fan-out is the orchestrator's job.
    GraphCalendar {
        /// The account.
        account: AccountId,
        /// The calendar.
        calendar: CalendarId,
    },
    /// A Gmail **account-global** message scope.
    ///
    /// Unlike Graph mail (`delta` per folder) and IMAP (state per mailbox), Gmail's
    /// `historyId` is an **account-wide** incremental cursor (like JMAP's per-account
    /// `Email` state), so all of an account's messages sync under one scope — there is
    /// no per-label fan-out. Gmail labels are multi-membership on the message itself,
    /// synced under [`GmailLabelList`](Self::GmailLabelList); a message's membership is
    /// its `labelIds`, not the scope it was fetched under.
    GmailMessages {
        /// The account.
        account: AccountId,
    },
    /// A Gmail per-account label-list (label discovery) scope.
    ///
    /// Like [`GraphFolderList`](Self::GraphFolderList), the label list is re-discovered
    /// as a snapshot each pass (`GET /users/me/labels`), so it carries no cursor of its
    /// own — but it is a distinct **container** scope, claimed and applied before the
    /// account's [`GmailMessages`](Self::GmailMessages) that reference its labels
    /// (`store-and-sync.md` referential apply order).
    GmailLabelList {
        /// The account.
        account: AccountId,
    },
    /// A Google Calendar per-account calendar-list (calendar discovery) scope.
    ///
    /// Like [`GmailLabelList`](Self::GmailLabelList), the calendar list is
    /// re-discovered as a snapshot each pass (`GET /calendar/v3/users/me/calendarList`),
    /// so it carries no cursor of its own — but it is a distinct **container** scope,
    /// claimed and applied before the per-calendar
    /// [`GoogleCalendar`](Self::GoogleCalendar) event scopes it parents.
    GoogleCalendarList {
        /// The account.
        account: AccountId,
    },
    /// A Google Calendar `(account, calendar)` event scope.
    ///
    /// Google Calendar `events.list` returns a per-calendar `nextSyncToken` cursor and
    /// (unlike Graph's *mandatory* `calendarView` window) an **optional** `timeMin`, so
    /// — like [`GraphCalendar`](Self::GraphCalendar) — a Google calendar provider is
    /// bound to one calendar for events; the cross-calendar fan-out is the
    /// orchestrator's job.
    GoogleCalendar {
        /// The account.
        account: AccountId,
        /// The calendar.
        calendar: CalendarId,
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
            | Self::GraphFolder { account, .. }
            | Self::GraphCalendarList { account }
            | Self::GraphCalendar { account, .. }
            | Self::GmailMessages { account }
            | Self::GmailLabelList { account }
            | Self::GoogleCalendarList { account }
            | Self::GoogleCalendar { account, .. } => account,
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
            // Graph mirrors IMAP: a per-folder message scope + a folder-list container.
            // Gmail's message scope is account-global (like JMAP's Email) but still a
            // message scope; its label list is the mailbox container.
            Self::ImapMailbox { .. } | Self::GraphFolder { .. } | Self::GmailMessages { .. } => {
                Some(ObjectKind::Message)
            }
            Self::ImapMailboxList { .. }
            | Self::GraphFolderList { .. }
            | Self::GmailLabelList { .. } => Some(ObjectKind::Mailbox),
            // Graph/Google calendar mirror CalDAV: a per-calendar event scope + a
            // calendar-list container.
            Self::DavCollection { .. }
            | Self::GraphCalendar { .. }
            | Self::GoogleCalendar { .. } => Some(ObjectKind::Event),
            Self::DavCollectionList { .. }
            | Self::GraphCalendarList { .. }
            | Self::GoogleCalendarList { .. } => Some(ObjectKind::Calendar),
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
#[path = "scope_tests.rs"]
mod tests;
