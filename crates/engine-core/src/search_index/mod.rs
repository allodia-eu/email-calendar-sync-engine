//! Projecting normalized objects into search-index rows.
//!
//! The store is **mechanical**: it writes derived rows, it never computes them
//! (`store-and-sync.md`). This module is the pure, I/O-free counterpart — it turns
//! a normalized [`Message`](crate::mail::Message) or
//! [`Event`](crate::calendar::Event) into the rows the store persists: the
//! full-text document and the structured filter rows (scalars, address/participant
//! junctions, and membership). The sync/ingestion path calls these projections
//! *before* the store write and carries the result in `engine-store`'s
//! `DerivedWrite`.
//!
//! Why these rows live here, not in `engine-store`: the projection reads the
//! domain model, so it is engine-core logic, and `engine-store` cannot depend on
//! it the other way around. The row *types* therefore live beside the projection,
//! and `DerivedWrite` (the atomic-write carrier) composes them. The one derived
//! row this module does **not** produce is the calendar `OccurrenceRow`: expanding
//! recurrence to UTC instants needs bundled tzdata and is not engine-core work
//! (`calendar-semantics.md`), so that type stays with the store carrier.
//!
//! The DSL→row mapping (which filter hits which table) is fixed in
//! `north-star.md`'s Search Contract and realized by the `store-sqlite` V2 schema.
//!
//! # Shape
//!
//! - [`FtsField`]/[`FtsRow`] — the field-tagged full-text document for one object.
//! - [`MembershipRow`]/[`MembershipKind`] — collection/keyword membership, shared
//!   by mail and calendar.
//! - [`mail`] — [`project_message`] and the mail filter rows.
//! - [`calendar`] — [`project_event`] (which needs the account's [`OwnerAddresses`]
//!   to resolve "my" RSVP) and the calendar filter rows.

pub mod calendar;
pub mod mail;

pub use calendar::{
    EventIndexRow, EventParticipantRow, EventProjection, OwnerAddresses, ParticipantField,
    project_event,
};
pub use mail::{AddressField, MailAddressRow, MailIndexRow, MailProjection, project_message};

use serde::{Deserialize, Serialize};

use crate::ids::ProviderKey;

/// Field-tagged searchable text for one object — one logical FTS document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtsField {
    /// The field name (e.g. `subject`, `body`, `location`; later
    /// `attachment:report.pdf`).
    pub name: String,
    /// The field's text.
    pub text: String,
}

impl FtsField {
    /// Creates a named full-text field.
    #[must_use]
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
        }
    }
}

/// A full-text row to upsert: the searchable text for one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtsRow {
    /// The object this text belongs to.
    pub key: ProviderKey,
    /// Field-tagged text segments, mapped by the store onto its native FTS engine
    /// (SQLite FTS5 columns, Postgres `tsvector`). Open-ended so indexed
    /// attachment text — a later, server-side capability — can be added as further
    /// fields without a schema change; the store folds unknown field names into a
    /// general text column.
    pub fields: Vec<FtsField>,
}

impl FtsRow {
    /// Creates a full-text row for an object key.
    #[must_use]
    pub fn new(key: ProviderKey, fields: Vec<FtsField>) -> Self {
        Self { key, fields }
    }
}

/// The collection axis a [`MembershipRow`] indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MembershipKind {
    /// A mailbox, folder, or label. The model unifies these as one membership
    /// axis (`modeling.md`), so the DSL's `mailbox:` and `label:` operators both
    /// query this kind.
    Mailbox,
    /// A message keyword (`$flagged`, `$seen`, or a user keyword).
    Keyword,
    /// A calendar.
    Calendar,
}

/// A membership-junction row: the object belongs to `value` under `kind`.
///
/// `value` is the collection's provider id (a [`crate::ids::MailboxId`] /
/// [`crate::ids::CalendarId`] string) or the keyword string. Resolving a
/// human-friendly name (e.g. "Inbox") to a provider id is a host/query-time
/// concern; the index stores ids, so account-scoped filtering stays exact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipRow {
    /// The object that holds this membership.
    pub key: ProviderKey,
    /// The membership axis.
    pub kind: MembershipKind,
    /// The collection id or keyword.
    pub value: String,
}

/// Normalizes an email address for case-insensitive index matching: trimmed and
/// lowercased. Returns an empty string for an address that is only whitespace,
/// which callers drop.
///
/// Both the projection (storage side) and a store's query executor must normalize
/// the same way, so a query address matches the stored one; this is the single
/// shared definition.
#[must_use]
pub fn normalize_addr(addr: &str) -> String {
    addr.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn fts_row_roundtrips_through_json() {
        let row = FtsRow::new(
            key("m1"),
            vec![
                FtsField::new("subject", "hi"),
                FtsField::new("body", "there"),
            ],
        );
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(serde_json::from_str::<FtsRow>(&json).unwrap(), row);
    }

    #[test]
    fn membership_row_roundtrips_and_distinguishes_kinds() {
        let mailbox = MembershipRow {
            key: key("m1"),
            kind: MembershipKind::Mailbox,
            value: "inbox".into(),
        };
        let keyword = MembershipRow {
            key: key("m1"),
            kind: MembershipKind::Keyword,
            value: "$flagged".into(),
        };
        assert_ne!(mailbox, keyword);
        let json = serde_json::to_string(&mailbox).unwrap();
        assert_eq!(
            serde_json::from_str::<MembershipRow>(&json).unwrap(),
            mailbox
        );
    }

    #[test]
    fn addresses_are_trimmed_and_lowercased() {
        assert_eq!(normalize_addr("  Alice@Example.COM "), "alice@example.com");
        assert_eq!(normalize_addr("   "), "");
    }
}
