//! The two contact reads that name a *source card* rather than a unified person.
//!
//! [`super::contacts`] answers "who is this person"; these answer the questions a host has to
//! settle before it can write one. A create needs a destination, and a destination is an
//! address book on one account. An edit needs the exact stored card its values came from,
//! because a person is several cards and writing a merged person's values back would file one
//! account's details in another's book.

use engine_core::{
    contact::AddressBook,
    ids::{AccountId, PersonId},
    people::PersonSource,
    sync::ObjectKind,
};
use engine_store::ContactStore;

use super::decode_error;
use crate::{ApiError, Engine};

impl Engine {
    /// Lists one account's synced address books, the contacts counterpart of
    /// [`Engine::calendars`].
    ///
    /// Every discovered book, writable or not: a host filters by
    /// [`AddressBook::is_writable`] for a save destination and lists the rest as sources.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn address_books(&self, account: &AccountId) -> Result<Vec<AddressBook>, ApiError> {
        let mut books = Vec::new();
        for payload in self.objects_of(account, ObjectKind::AddressBook).await? {
            books.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(books)
    }

    /// The live source cards one person was assembled from, ordered by `(account, card)`.
    ///
    /// Resolves a retired id through the alias table exactly as [`Engine::person`] does, so a
    /// row a host is still holding after a merge names the surviving person's cards. A person
    /// that no longer exists yields an empty vector, which is also what a person whose every
    /// card has since been tombstoned yields; the two are indistinguishable here and equally
    /// mean "there is nothing left to edit".
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when the people or source snapshot cannot be read.
    pub async fn person_sources(&self, id: PersonId) -> Result<Vec<PersonSource>, ApiError> {
        let Some(person) = self.store.people_snapshot().await?.resolve(id).cloned() else {
            return Ok(Vec::new());
        };
        let mut sources: Vec<PersonSource> = self
            .store
            .contact_sources()
            .await?
            .sources
            .into_iter()
            .filter(|source| person.sources.contains(&source.id))
            .collect();
        sources.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(sources)
    }
}
