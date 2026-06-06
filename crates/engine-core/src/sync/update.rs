//! Sync updates.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ids::ProviderKey;

/// A normalized batch of provider changes for one scope, produced by an adapter
/// and applied atomically by the store.
///
/// It is **either a delta or a snapshot** (`store-and-sync.md`):
///
/// - A [`SyncUpdate::Delta`] lists changed objects and explicitly removed keys.
/// - A [`SyncUpdate::Snapshot`] carries the **complete** current provider-id set
///   for the scope in `present`; the store tombstones any local row in the scope
///   whose key is absent from `present`. `cannotCalculateChanges` (JMAP) and a
///   `UIDVALIDITY` reset (IMAP) produce snapshots, not deltas.
///
/// `T` is the normalized object type for the scope (a message, event, mailbox,
/// or calendar). Removed/present keys use the universal [`ProviderKey`], which
/// is how the store keys its rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncUpdate<T> {
    /// An incremental change set.
    Delta {
        /// Objects created or updated since the previous cursor.
        changed: Vec<T>,
        /// Keys of objects destroyed since the previous cursor.
        removed: Vec<ProviderKey>,
    },
    /// A bounded or full snapshot whose `present` set drives tombstoning.
    Snapshot {
        /// The objects carried by this snapshot.
        objects: Vec<T>,
        /// The complete current set of provider keys in the scope. Any local key
        /// not in this set is tombstoned.
        present: BTreeSet<ProviderKey>,
    },
}

impl<T> SyncUpdate<T> {
    /// Creates a delta update.
    #[must_use]
    pub fn delta(changed: Vec<T>, removed: Vec<ProviderKey>) -> Self {
        Self::Delta { changed, removed }
    }

    /// Creates a snapshot update.
    #[must_use]
    pub fn snapshot(objects: Vec<T>, present: BTreeSet<ProviderKey>) -> Self {
        Self::Snapshot { objects, present }
    }

    /// Returns `true` if this update is a snapshot (so the store must tombstone
    /// local rows absent from `present`).
    #[must_use]
    pub fn is_snapshot(&self) -> bool {
        matches!(self, Self::Snapshot { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn delta_lists_changed_and_removed() {
        let update: SyncUpdate<String> = SyncUpdate::delta(vec!["a".to_owned()], vec![key("b")]);
        assert!(!update.is_snapshot());
        assert_eq!(
            update,
            SyncUpdate::Delta {
                changed: vec!["a".to_owned()],
                removed: vec![key("b")],
            },
        );
    }

    #[test]
    fn snapshot_carries_full_present_set() {
        let present: BTreeSet<ProviderKey> = [key("x"), key("y")].into_iter().collect();
        let update: SyncUpdate<String> =
            SyncUpdate::snapshot(vec!["x".to_owned()], present.clone());
        assert!(update.is_snapshot());
        assert_eq!(
            update,
            SyncUpdate::Snapshot {
                objects: vec!["x".to_owned()],
                present,
            },
        );
    }

    #[test]
    fn roundtrips_through_json() {
        let update: SyncUpdate<String> = SyncUpdate::delta(vec!["a".to_owned()], vec![key("b")]);
        let json = serde_json::to_string(&update).unwrap();
        assert_eq!(
            serde_json::from_str::<SyncUpdate<String>>(&json).unwrap(),
            update
        );
    }
}
