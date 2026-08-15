//! Sync updates.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    calendar::{Calendar, Event},
    contact::{AddressBook, ContactCard},
    ids::ProviderKey,
    mail::{MailContent, MailStateChange, Mailbox, Message},
};

/// An object — or a change to one — identified by the provider key it concerns.
///
/// One accessor, so a sync carrier can key whatever it holds without knowing whether that is a
/// whole object or a partial change to one.
pub trait Keyed {
    /// The provider key this concerns.
    fn provider_key(&self) -> &ProviderKey;
}

impl Keyed for Message {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl Keyed for Mailbox {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl Keyed for Event {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl Keyed for Calendar {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl Keyed for ContactCard {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl Keyed for AddressBook {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl Keyed for MailStateChange {
    fn provider_key(&self) -> &ProviderKey {
        &self.key
    }
}

impl Keyed for NoPatch {
    fn provider_key(&self) -> &ProviderKey {
        // Uninhabited: there is no value of this type to ask.
        match *self {}
    }
}

/// A normalized object a sync pass reports changes to.
///
/// The associated [`Patch`](SyncObject::Patch) is the **partial** form a provider may report in
/// place of the whole object, when it can say that only some fields moved. Writing a partial
/// costs the columns it names, and — because it never claims to carry the rest — it cannot
/// destroy a field the provider had no way to send.
///
/// An object with no partial form uses [`NoPatch`], which has no values: `Vec<NoPatch>` is
/// provably empty, so *the compiler* says a calendar pass carries no partials, rather than a
/// comment asking the reader to believe it.
pub trait SyncObject: Keyed + Serialize {
    /// The partial change form for this object, or [`NoPatch`] if it has none.
    type Patch: Keyed
        + core::fmt::Debug
        + Clone
        + PartialEq
        + Eq
        + Serialize
        + DeserializeOwned
        + Send
        + Sync;

    /// The JSON a store persists for this object.
    ///
    /// Defaults to the whole object. [`Message`] overrides it to persist a [`MailContent`] —
    /// everything the provider said about the message's content, and none of the state whose
    /// home is the message row — so a stored payload cannot disagree with that row.
    ///
    /// # Errors
    ///
    /// Returns the serializer's error if the object cannot be represented as JSON.
    fn to_payload(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

/// The patch type of an object with no partial form.
///
/// Uninhabited on purpose: there is no value of this type, so a `Vec<NoPatch>` can only ever be
/// empty and every `match` over one is exhaustive with no arms. A calendar or contact pass
/// carrying no partials is therefore a fact the compiler checks, not a convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoPatch {}

impl SyncObject for Message {
    type Patch = MailStateChange;

    fn to_payload(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(MailContent::from(self))
    }
}

impl SyncObject for Mailbox {
    type Patch = NoPatch;
}

impl SyncObject for Event {
    type Patch = NoPatch;
}

impl SyncObject for Calendar {
    type Patch = NoPatch;
}

impl SyncObject for ContactCard {
    type Patch = NoPatch;
}

impl SyncObject for AddressBook {
    type Patch = NoPatch;
}

/// A normalized batch of provider changes for one scope, produced by an adapter
/// and applied atomically by the store.
///
/// It is **either a delta or a snapshot** (`store-and-sync.md`):
///
/// - A [`SyncUpdate::Delta`] lists changed objects, partial changes, and explicitly removed keys.
/// - A [`SyncUpdate::Snapshot`] carries the **complete** current provider-id set for the scope in
///   `present`; the store tombstones any local row in the scope whose key is absent from `present`.
///   `cannotCalculateChanges` (JMAP) and a `UIDVALIDITY` reset (IMAP) produce snapshots, not
///   deltas.
///
/// A snapshot carries no partials: it is by definition the scope's whole current state, so every
/// object in it is whole.
///
/// `T` is the normalized object type for the scope (a message, event, mailbox,
/// or calendar). Removed/present keys use the universal [`ProviderKey`], which
/// is how the store keys its rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncUpdate<T: SyncObject> {
    /// An incremental change set.
    Delta {
        /// Objects created or updated **in full** since the previous cursor.
        changed: Vec<T>,
        /// Objects the provider reported a **partial** change for — only the fields the patch
        /// names moved.
        ///
        /// A key appearing in both `changed` and here is resolved in favour of `changed`: a
        /// whole object is strictly more information, and an adapter fetches it *after* learning
        /// the change, so it is the later word. Gmail's history API can report both for one id
        /// in a single page, so this is a real case, not a hypothetical.
        patched: Vec<T::Patch>,
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

impl<T: SyncObject> SyncUpdate<T> {
    /// Creates a delta update carrying whole objects and removals.
    #[must_use]
    pub fn delta(changed: Vec<T>, removed: Vec<ProviderKey>) -> Self {
        Self::Delta {
            changed,
            patched: Vec::new(),
            removed,
        }
    }

    /// Attaches the partial changes this delta carries, **dropping any whose object is already
    /// here in full**. A no-op on a snapshot, which by definition carries whole objects only.
    ///
    /// The drop is the point: a provider can report both for one key in a single pass — Gmail's
    /// history API does, when a message arrives and is labelled between two cursors — and a
    /// whole object is both strictly more information and the later word, since the adapter
    /// fetched it after learning of the change. Resolving that here means no adapter has to
    /// remember to, and no store has to guess which write wins.
    #[must_use]
    pub fn with_patched(mut self, patches: Vec<T::Patch>) -> Self {
        if let Self::Delta {
            changed, patched, ..
        } = &mut self
        {
            let whole: BTreeSet<&ProviderKey> = changed.iter().map(Keyed::provider_key).collect();
            *patched = patches
                .into_iter()
                .filter(|patch| !whole.contains(patch.provider_key()))
                .collect();
        }
        self
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

    /// The whole objects this update carries (a delta's `changed` or a snapshot's `objects`).
    #[must_use]
    pub fn changed(&self) -> &[T] {
        match self {
            Self::Delta { changed, .. } => changed,
            Self::Snapshot { objects, .. } => objects,
        }
    }

    /// The partial changes this update carries; empty for a snapshot.
    #[must_use]
    pub fn patched(&self) -> &[T::Patch] {
        match self {
            Self::Delta { patched, .. } => patched,
            Self::Snapshot { .. } => &[],
        }
    }

    /// The explicitly-removed keys (empty for a snapshot, whose removals are computed by
    /// present-set diff inside the store).
    #[must_use]
    pub fn removed(&self) -> &[ProviderKey] {
        match self {
            Self::Delta { removed, .. } => removed,
            Self::Snapshot { .. } => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::{MailboxId, MessageId},
        mail::{Keyword, SystemKeyword},
        membership::Memberships,
    };

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    fn message(id: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        )
    }

    #[test]
    fn delta_lists_changed_and_removed() {
        let update: SyncUpdate<Message> = SyncUpdate::delta(vec![message("a")], vec![key("b")]);
        assert!(!update.is_snapshot());
        assert_eq!(update.changed().len(), 1);
        assert_eq!(update.removed(), [key("b")]);
        assert!(update.patched().is_empty());
    }

    #[test]
    fn a_delta_carries_partial_changes_beside_whole_objects() {
        let patch = MailStateChange::keywords(
            key("c"),
            [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
        );
        let update: SyncUpdate<Message> =
            SyncUpdate::delta(vec![message("a")], vec![]).with_patched(vec![patch.clone()]);
        assert_eq!(update.changed().len(), 1);
        assert_eq!(update.patched(), [patch]);
    }

    #[test]
    fn a_whole_object_supersedes_a_patch_for_the_same_key() {
        // Gmail's history API reports a message arriving and then being labelled between two
        // cursors, so one page carries both. The whole object was fetched after the label
        // change was learned, so it is the later word — and applying the patch on top of it
        // would write keywords the object had already superseded.
        let update: SyncUpdate<Message> = SyncUpdate::delta(vec![message("a")], vec![])
            .with_patched(vec![
                MailStateChange::keywords(key("a"), BTreeSet::new()),
                MailStateChange::keywords(key("b"), BTreeSet::new()),
            ]);
        assert_eq!(
            update
                .patched()
                .iter()
                .map(|p| p.key.as_str())
                .collect::<Vec<_>>(),
            vec!["b"],
            "the patch for the key that also arrived whole is dropped; the other survives"
        );
    }

    #[test]
    fn a_snapshot_carries_no_partials() {
        // Not merely empty by convention: a snapshot is the scope's whole current state, so
        // there is nothing a partial could mean. `with_patched` cannot smuggle one in.
        let present: BTreeSet<ProviderKey> = [key("x")].into_iter().collect();
        let update: SyncUpdate<Message> = SyncUpdate::snapshot(vec![message("x")], present)
            .with_patched(vec![MailStateChange::keywords(key("x"), BTreeSet::new())]);
        assert!(update.is_snapshot());
        assert!(update.patched().is_empty());
    }

    #[test]
    fn an_object_with_no_partial_form_cannot_carry_one() {
        // `Event::Patch` is `NoPatch`, which is uninhabited: the only vector this line can be
        // written with is the empty one.
        let update: SyncUpdate<Event> = SyncUpdate::delta(vec![], vec![]).with_patched(vec![]);
        assert!(update.patched().is_empty());
    }

    #[test]
    fn every_synced_object_keys_itself_by_its_own_id() {
        use crate::{
            calendar::Calendar,
            ids::CalendarId,
            mail::{Keyword, Mailbox, SystemKeyword},
        };

        assert_eq!(
            Mailbox::new(MailboxId::try_from("inbox").unwrap(), "Inbox")
                .provider_key()
                .as_str(),
            "inbox"
        );
        assert_eq!(
            Calendar::new(CalendarId::try_from("work").unwrap(), "Work")
                .provider_key()
                .as_str(),
            "work"
        );
        // A patch keys itself the same way, which is what lets one carrier hold both.
        let patch = MailStateChange::keywords(
            key("m1"),
            [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
        );
        assert_eq!(patch.provider_key().as_str(), "m1");
    }

    #[test]
    fn roundtrips_through_json() {
        let update: SyncUpdate<Message> = SyncUpdate::delta(vec![message("a")], vec![key("b")])
            .with_patched(vec![MailStateChange::keywords(key("c"), BTreeSet::new())]);
        let json = serde_json::to_string(&update).unwrap();
        assert_eq!(
            serde_json::from_str::<SyncUpdate<Message>>(&json).unwrap(),
            update
        );
    }
}
