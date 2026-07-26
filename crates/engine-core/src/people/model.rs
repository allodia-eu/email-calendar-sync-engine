//! Unified-person inputs and materialized output.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::CanonicalEmail;
use crate::{
    contact::{ContactCard, ContactKind, ContactSourceClass},
    ids::{AccountId, ContactId, PersonId},
};

/// The account-scoped source record backing a person value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PersonSourceId {
    /// Account containing the provider record.
    pub account: AccountId,
    /// Provider contact id.
    pub contact: ContactId,
}

impl PersonSourceId {
    /// Creates a source id.
    #[must_use]
    pub fn new(account: AccountId, contact: ContactId) -> Self {
        Self { account, contact }
    }
}

/// One provider card supplied to the people derivation pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonSource {
    /// Source identity.
    pub id: PersonSourceId,
    /// Lossless normalized provider card.
    pub card: ContactCard,
    /// Source authority class.
    pub source_class: ContactSourceClass,
    /// Whether this record can be edited at its source.
    pub writable: bool,
}

impl PersonSource {
    /// Creates a source from an account and card.
    #[must_use]
    pub fn new(
        account: AccountId,
        mut card: ContactCard,
        source_class: ContactSourceClass,
        writable: bool,
    ) -> Self {
        card.source_class = source_class;
        card.is_writable = writable;
        let id = PersonSourceId::new(account, card.id.clone());
        Self {
            id,
            card,
            source_class,
            writable,
        }
    }
}

/// A unioned value and every provider record that supplied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcedValue<T: Ord> {
    /// The exact normalized value.
    pub value: T,
    /// Source records carrying the value.
    pub sources: BTreeSet<PersonSourceId>,
}

/// One materialized cross-account person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    /// Stable store-local person id.
    pub id: PersonId,
    /// Deterministically selected display name: the best-ranked source name, else the
    /// first canonical email.
    ///
    /// `None` when the sources carry neither — a card with no name and no address. The
    /// engine deliberately invents nothing here: any placeholder it chose ("Unnamed
    /// contact") would be untranslatable English baked into a provider-neutral core and
    /// surfaced verbatim by every host. Naming the nameless is a presentation decision,
    /// so it belongs to the host, which knows the user's language.
    pub display_name: Option<String>,
    /// All source cards in this connected component.
    pub sources: BTreeSet<PersonSourceId>,
    /// Card kinds represented by the source records.
    pub kinds: BTreeSet<ContactKind>,
    /// All names with provenance.
    pub names: Vec<SourcedValue<String>>,
    /// All canonical emails with provenance.
    pub emails: Vec<SourcedValue<CanonicalEmail>>,
    /// All phones with provenance.
    pub phones: Vec<SourcedValue<String>>,
    /// All organization names with provenance.
    pub organizations: Vec<SourcedValue<String>>,
    /// All titles/roles with provenance.
    pub titles: Vec<SourcedValue<String>>,
    /// Whether at least one personal source is saved.
    pub is_saved: bool,
    /// Whether at least one source is writable.
    pub is_writable: bool,
}

/// A complete materialized people generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeopleSnapshot {
    /// Materialized people in deterministic source-component order.
    pub people: Vec<Person>,
    /// Retired person ids resolving to their surviving id.
    pub aliases: BTreeMap<PersonId, PersonId>,
    /// Next never-issued store-local id.
    pub next_id: u64,
}

impl PeopleSnapshot {
    /// Creates an empty initial snapshot.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            people: Vec::new(),
            aliases: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Resolves a current or retired person id.
    #[must_use]
    pub fn resolve(&self, mut id: PersonId) -> Option<&Person> {
        let mut remaining = self.aliases.len().saturating_add(1);
        while let Some(next) = self.aliases.get(&id) {
            id = *next;
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                return None;
            }
        }
        self.people.iter().find(|person| person.id == id)
    }
}

impl Default for PeopleSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}
