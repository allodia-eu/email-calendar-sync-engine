//! Collection membership.
//!
//! Provider object identity and collection membership are separate axes. A mail
//! object belongs to one or more mailboxes/labels; a calendar event belongs to
//! one or more calendars. Both share the invariant that the set is **never
//! empty** (JMAP RFC 8621 §2 for `mailboxIds`, the JMAP Calendars draft for
//! `calendarIds`), which [`Memberships`] enforces by construction so the
//! containing object can expose it as a plain field.
//!
//! IMAP copies in different folders are *distinct* provider objects, each with a
//! single-element membership; JMAP/Gmail objects carry a multi-element one. The
//! same type models both.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use serde::de::{self, Deserialize, Deserializer};
use serde::{Serialize, Serializer};

/// Error returned when a membership set would be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a membership set must contain at least one collection")]
pub struct EmptyMembership;

/// A non-empty set of collection ids an object belongs to.
///
/// The non-empty invariant is enforced on construction and preserved by the API:
/// there is no operation that can empty the set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Memberships<T: Ord>(BTreeSet<T>);

impl<T: Ord> Memberships<T> {
    /// Builds a membership set from an iterator of collection ids.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyMembership`] if the iterator yields no ids.
    pub fn new(items: impl IntoIterator<Item = T>) -> Result<Self, EmptyMembership> {
        let set: BTreeSet<T> = items.into_iter().collect();
        if set.is_empty() {
            Err(EmptyMembership)
        } else {
            Ok(Self(set))
        }
    }

    /// Builds a single-membership set — the common case for IMAP objects and the
    /// default for new objects.
    #[must_use]
    pub fn of_one(item: T) -> Self {
        let mut set = BTreeSet::new();
        set.insert(item);
        Self(set)
    }

    /// Adds a collection id, returning `true` if it was not already present.
    pub fn insert(&mut self, item: T) -> bool {
        self.0.insert(item)
    }

    /// Returns `true` if the object belongs to the given collection.
    #[must_use]
    pub fn contains(&self, item: &T) -> bool {
        self.0.contains(item)
    }

    /// Returns the number of collections, which is always at least one.
    #[must_use]
    pub fn len(&self) -> NonZeroUsize {
        // The set is non-empty by construction, so the fallback is unreachable;
        // using `unwrap_or` keeps the method panic-free.
        NonZeroUsize::new(self.0.len()).unwrap_or(NonZeroUsize::MIN)
    }

    /// Iterates over the collection ids in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }

    /// Returns the underlying set.
    #[must_use]
    pub fn as_set(&self) -> &BTreeSet<T> {
        &self.0
    }
}

impl<T: Ord + Serialize> Serialize for Memberships<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Ord + Deserialize<'de>> Deserialize<'de> for Memberships<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let set = BTreeSet::<T>::deserialize(deserializer)?;
        Self::new(set).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MailboxId;

    fn mailbox(id: &str) -> MailboxId {
        MailboxId::try_from(id).unwrap()
    }

    #[test]
    fn empty_membership_is_rejected() {
        assert_eq!(
            Memberships::<MailboxId>::new(Vec::new()),
            Err(EmptyMembership)
        );
    }

    #[test]
    fn single_and_multi_membership() {
        let one = Memberships::of_one(mailbox("inbox"));
        assert_eq!(one.len().get(), 1);
        assert!(one.contains(&mailbox("inbox")));

        let mut many = Memberships::new([mailbox("inbox"), mailbox("important")]).unwrap();
        assert_eq!(many.len().get(), 2);
        assert!(!many.insert(mailbox("inbox"))); // already present
        assert!(many.insert(mailbox("starred")));
        assert_eq!(many.len().get(), 3);
    }

    #[test]
    fn deserialization_enforces_non_empty() {
        let json = serde_json::to_string(&Memberships::of_one(mailbox("inbox"))).unwrap();
        assert_eq!(json, "[\"inbox\"]");
        let back: Memberships<MailboxId> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len().get(), 1);

        let err = serde_json::from_str::<Memberships<MailboxId>>("[]").unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }
}
