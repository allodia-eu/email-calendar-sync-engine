//! Outbound calendar write shapes (CalDAV `PUT` / `DELETE`).
//!
//! These mirror the mail [`Draft`](crate::Draft)/[`SubmissionReceipt`](crate::SubmissionReceipt)
//! pair: a serializable request a caller stores as a durable outbox `PendingOp`
//! payload before the side effect, plus a receipt the outbox records on success.
//!
//! The body is the **preserved/round-tripped [`RawIcal`]**, never a
//! re-serialization of the lossy projection (`calendar-semantics.md` — "provider
//! writes round-trip from raw plus targeted patches"): a create carries a freshly
//! built iCalendar document, an update carries the stored `raw_ical` with targeted
//! patches applied. Optimistic concurrency rides on the CalDAV `ETag` via an
//! `If-Match`/`If-None-Match` precondition, so a stale edit can never silently
//! clobber a newer server copy (RFC 4791 §5.3.2, RFC 7232).

use engine_core::ids::{EventId, ProviderKey, Uid};
use engine_core::raw::RawIcal;
use engine_core::version::ETag;
use serde::{Deserialize, Serialize};

/// The optimistic-concurrency precondition for a calendar `PUT` (HTTP conditional
/// request, RFC 7232; CalDAV RFC 4791 §5.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WritePrecondition {
    /// `If-None-Match: *` — the write must **create** a new resource; the server
    /// rejects it (`412 Precondition Failed`) if the href already exists. Use for a
    /// create so a retry can never silently overwrite a resource a concurrent
    /// writer created at the same href.
    IfNoneMatch,
    /// `If-Match: "<etag>"` — the write must **replace** the existing resource only
    /// while its server copy still carries this [`ETag`]; otherwise the server
    /// rejects it (`412`), so a stale edit never clobbers a newer one (refetch and
    /// merge).
    IfMatch(ETag),
    /// No conditional header — an unconditional create-or-replace. Use only when no
    /// concurrency guarantee is needed; prefer the conditional variants.
    Unconditional,
}

/// A request to create or replace a calendar object resource via `PUT`
/// (RFC 4791 §5.3.2).
///
/// `href` is the resource key the object will have (its [`EventId`]); for a create
/// the caller mints it (commonly `<collection>/<uid>.ics` — see
/// `CalDavProvider::event_href`). The cross-system [`Uid`] is echoed on the receipt
/// for reconciliation. Serializable so it can be stored as a durable outbox payload
/// before the `PUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventWrite {
    /// The target resource href (the object's [`EventId`]).
    pub href: EventId,
    /// The event's cross-system `UID`, echoed on the receipt for reconciliation.
    pub uid: Uid,
    /// The iCalendar document to store (round-tripped from raw, not re-serialized).
    pub ical: RawIcal,
    /// The optimistic-concurrency precondition.
    pub precondition: WritePrecondition,
}

impl EventWrite {
    /// A **create**: `PUT` with `If-None-Match: *`, so it never overwrites an
    /// existing resource at `href`.
    #[must_use]
    pub fn create(href: EventId, uid: Uid, ical: RawIcal) -> Self {
        Self {
            href,
            uid,
            ical,
            precondition: WritePrecondition::IfNoneMatch,
        }
    }

    /// An **update**: `PUT` with `If-Match: <etag>`, so it replaces the resource
    /// only while the server copy still carries `etag`.
    #[must_use]
    pub fn update(href: EventId, uid: Uid, ical: RawIcal, etag: ETag) -> Self {
        Self {
            href,
            uid,
            ical,
            precondition: WritePrecondition::IfMatch(etag),
        }
    }
}

/// A request to delete a calendar object resource via `DELETE` (RFC 4791).
///
/// Serializable so it can be stored as a durable outbox payload before the delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDeletion {
    /// The resource href to delete (the object's [`EventId`]).
    pub href: EventId,
    /// The `If-Match` ETag guarding the delete; `None` deletes unconditionally.
    pub etag: Option<ETag>,
}

impl EventDeletion {
    /// A delete guarded by `If-Match: <etag>` (fails `412` if the server copy moved
    /// on).
    #[must_use]
    pub fn if_match(href: EventId, etag: ETag) -> Self {
        Self {
            href,
            etag: Some(etag),
        }
    }

    /// An unconditional delete (no `If-Match` guard).
    #[must_use]
    pub fn unconditional(href: EventId) -> Self {
        Self { href, etag: None }
    }
}

/// The result of a successful calendar `PUT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWriteReceipt {
    /// The provider key now backing the object (the resource href), for the outbox
    /// to record as the op's resolved key.
    pub event_key: ProviderKey,
    /// The event's `UID`, echoed for sync-time reconciliation.
    pub uid: Uid,
    /// The new [`ETag`] **if the server returned one** on the `PUT` (RFC 4791
    /// §5.3.4 recommends it); `None` means the caller must learn the new revision
    /// from the next sync (a `sync-collection` delta carries it).
    pub etag: Option<ETag>,
}

impl EventWriteReceipt {
    /// Records a successful write.
    #[must_use]
    pub fn new(event_key: ProviderKey, uid: Uid, etag: Option<ETag>) -> Self {
        Self {
            event_key,
            uid,
            etag,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn href() -> EventId {
        EventId::try_from("/dav/cal/alice/default/evt-1.ics").unwrap()
    }

    fn uid() -> Uid {
        Uid::new("evt-1@test.local").unwrap()
    }

    #[test]
    fn create_uses_if_none_match() {
        let write = EventWrite::create(
            href(),
            uid(),
            RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
        );
        assert_eq!(write.precondition, WritePrecondition::IfNoneMatch);
        assert_eq!(write.href, href());
        assert_eq!(write.uid, uid());
    }

    #[test]
    fn update_carries_the_if_match_etag() {
        let write = EventWrite::update(
            href(),
            uid(),
            RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
            ETag::new("\"v7\""),
        );
        assert_eq!(
            write.precondition,
            WritePrecondition::IfMatch(ETag::new("\"v7\""))
        );
    }

    #[test]
    fn deletion_guards_are_distinct() {
        let guarded = EventDeletion::if_match(href(), ETag::new("\"v7\""));
        let forced = EventDeletion::unconditional(href());
        assert_eq!(guarded.etag, Some(ETag::new("\"v7\"")));
        assert!(forced.etag.is_none());
        assert_ne!(guarded, forced);
    }

    #[test]
    fn receipt_records_the_new_etag() {
        let receipt = EventWriteReceipt::new(
            ProviderKey::new("/dav/cal/alice/default/evt-1.ics").unwrap(),
            uid(),
            Some(ETag::new("\"v8\"")),
        );
        assert_eq!(receipt.etag, Some(ETag::new("\"v8\"")));
        assert_eq!(receipt.uid, uid());
    }
}
