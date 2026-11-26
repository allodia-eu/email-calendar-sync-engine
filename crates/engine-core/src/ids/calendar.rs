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
    /// **distinct from the event's [`Uid`](super::Uid)**: in Google, every occurrence of a
    /// recurring series has a different `id` but they share one `iCalUID`; the
    /// `EventId` is per-provider-object, the `Uid` is the cross-system event
    /// identity used for scheduling reconciliation.
    EventId
}
