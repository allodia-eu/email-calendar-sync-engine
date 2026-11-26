//! The cross-system identity of an event or a contact.

use serde::{Deserialize, Serialize};

use super::IdError;

content_id! {
    /// The `UID` of an event or a contact: the identity one object keeps across systems, address
    /// books, calendars and views, where every provider key names only one server's copy.
    ///
    /// - An **event's** is the iCalendar / JSCalendar `UID` (RFC 5545 §3.8.4.7, RFC 8984
    ///   §4.1.2). The same `Uid` is shared by every recurrence instance of a series; an individual
    ///   instance is identified by `(Uid, recurrence-id)`. iTIP reconciliation keys on
    ///   `(Uid, SEQUENCE, RECURRENCE-ID)` (RFC 5546 §2.1.5), so this value is load-bearing for
    ///   scheduling and must never be truncated.
    /// - A **contact's** is the vCard `UID` (RFC 6350 §6.7.6) or the JSContact `uid` (RFC 9553
    ///   §2.1.9).
    ///
    /// A provider's own object id is never a `Uid`: it is an [`EventId`](super::EventId) or a
    /// [`ContactId`](super::ContactId). A source without a real UID gives `None` where a `Uid`
    /// is optional.
    ///
    /// The 1024-octet cap is an engine-level defense against hostile input; no RFC mandates a
    /// maximum length.
    Uid,
    max_octets = 1024
}
