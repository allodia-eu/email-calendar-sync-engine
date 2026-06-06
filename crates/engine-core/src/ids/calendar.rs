//! Calendar identity newtypes.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// Identifies a calendar collection within an account (JMAP `Calendar.id`,
    /// Google `calendarId`, CalDAV calendar-collection key, Graph `calendar.id`).
    /// A calendar carries access rights, subscription, owner, default reminders,
    /// and color — not only event membership.
    CalendarId
}

object_id! {
    /// Identifies a stored calendar event object within an account.
    ///
    /// This is the provider's object key (JMAP `CalendarEvent.id`, Google event
    /// `id`, CalDAV resource href, Graph `event.id` in immutable-id form). It is
    /// **distinct from the event's [`Uid`]**: in Google, every occurrence of a
    /// recurring series has a different `id` but they share one `iCalUID`; the
    /// `EventId` is per-provider-object, the `Uid` is the cross-system event
    /// identity used for scheduling reconciliation.
    EventId
}

content_id! {
    /// The iCalendar / JSCalendar `UID` (RFC 5545 §3.8.4.7, RFC 8984 §4.1.2):
    /// the globally unique, cross-system event identity.
    ///
    /// The same `Uid` is shared by every recurrence instance of a series; an
    /// individual instance is identified by `(Uid, recurrence-id)`. iTIP
    /// reconciliation keys on `(Uid, SEQUENCE, RECURRENCE-ID)` (RFC 5546
    /// §2.1.5), so this value is load-bearing for scheduling and must never be
    /// truncated.
    ///
    /// The 1024-octet cap is an engine-level defense against hostile input;
    /// neither RFC mandates a maximum length.
    Uid,
    max_octets = 1024
}
