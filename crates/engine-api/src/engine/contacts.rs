//! Host-facing people, recipient, and contact-write APIs. The photo fetch and its
//! cache keys live in [`super::contact_photo`].

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactKind, ContactPatch, ContactSourceClass},
    ids::{AccountId, AddressBookId, ContactId, PersonId},
    people::{CanonicalEmail, Person, PersonSource, PersonSourceId},
    recipient::{RecipientCoverage, RecipientSuggestion, rank_recipient_suggestions},
    sync::ObjectKind,
};
use engine_provider::{ContactDestination, ContactsProvider};
use engine_store::{ContactStore, StoreRead};
use engine_sync::{
    ContactReconcileReport, ContactWriteOutcome, create_contact, delete_contact, patch_contact,
    reconcile_contact_card, reconcile_contact_deletion,
};
use serde::{Deserialize, Serialize};

use super::{
    LEASE_TTL,
    contact_query::{
        PeopleCursor, decode_cursor, display_key, encode_cursor, person_key, person_matches,
        query_signature,
    },
    map_sync_error, worker,
};
use crate::{ApiError, Engine};

/// Filters and keyset cursor for one people page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeopleQuery {
    /// Name/email/phone/organization/title text.
    pub query: String,
    /// Account filter.
    pub account: Option<AccountId>,
    /// Address-book filter.
    pub address_book: Option<AddressBookId>,
    /// Source authority filter.
    pub source_class: Option<ContactSourceClass>,
    /// Card-kind filter.
    pub kind: Option<ContactKind>,
    /// Only people referenced by this synced group card.
    pub group: Option<ContactId>,
    /// Writable-source filter.
    pub writable: Option<bool>,
    /// Opaque cursor returned by the previous page.
    pub cursor: Option<String>,
    /// Page size. Zero uses 50; maximum 200.
    pub limit: usize,
}

/// Stable page from one people-index generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeoplePage {
    /// Matching people.
    pub people: Vec<Person>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
    /// Contact-source generation this page represents.
    pub generation: u64,
}

/// Autosuggest results plus honest mail-window coverage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipientSuggestions {
    /// Globally unique canonical-email results.
    pub suggestions: Vec<RecipientSuggestion>,
    /// Coverage per account.
    pub coverage: Vec<RecipientCoverage>,
}

/// A landed contact write and post-write store reconciliation.
#[derive(Debug)]
pub struct ContactWrite {
    /// Durable outbox outcome.
    pub write: ContactWriteOutcome,
    /// Whether the store now contains the server-canonical card.
    pub reconciled: ContactReconciled,
}

/// A landed contact deletion and post-write tombstone reconciliation.
#[derive(Debug)]
pub struct ContactDelete {
    /// Durable outbox operation.
    pub op: engine_core::write::PendingOpId,
    /// Whether the source row and people index were reconciled.
    pub reconciled: ContactReconciled,
}

/// Post-write contact reconciliation state.
#[derive(Debug)]
pub enum ContactReconciled {
    /// Source row and people generation updated.
    Applied(ContactReconcileReport),
    /// The contact sync scope was already leased.
    Busy,
    /// The provider write landed but local reconciliation failed.
    Failed(Box<ApiError>),
}

impl Engine {
    /// Returns matching unified people under a generation-bound keyset cursor.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for a malformed/stale cursor or
    /// [`ApiError::Store`] when the people snapshot cannot be read.
    pub async fn people_page(&self, query: &PeopleQuery) -> Result<PeoplePage, ApiError> {
        let source_snapshot = self.store.contact_sources().await?;
        let people = self.store.people_snapshot().await?;
        let sources: BTreeMap<PersonSourceId, PersonSource> = source_snapshot
            .sources
            .into_iter()
            .map(|source| (source.id.clone(), source))
            .collect();
        let signature = query_signature(query);
        let after = query.cursor.as_deref().map(decode_cursor).transpose()?;
        if let Some(cursor) = &after
            && (cursor.generation != source_snapshot.generation || cursor.signature != signature)
        {
            return Err(ApiError::InvalidInput(
                "people cursor does not match this query or generation".into(),
            ));
        }
        let needle = query.query.trim().to_lowercase();
        let mut matches: Vec<Person> = people
            .people
            .into_iter()
            .filter(|person| person_matches(person, &sources, query, &needle))
            .filter(|person| {
                after.as_ref().is_none_or(|cursor| {
                    person_key(person) > (cursor.display.clone(), cursor.person)
                })
            })
            .collect();
        matches.sort_by_key(person_key);
        let limit = if query.limit == 0 {
            50
        } else {
            query.limit.min(200)
        };
        let has_more = matches.len() > limit;
        matches.truncate(limit);
        let next_cursor = has_more
            .then(|| matches.last())
            .flatten()
            .map(|person| {
                encode_cursor(&PeopleCursor {
                    generation: source_snapshot.generation,
                    signature,
                    display: display_key(person),
                    person: person.id,
                })
            })
            .transpose()?;
        Ok(PeoplePage {
            people: matches,
            next_cursor,
            generation: source_snapshot.generation,
        })
    }

    /// Resolves a current or retired person id.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when the people snapshot cannot be read.
    pub async fn person(&self, id: PersonId) -> Result<Option<Person>, ApiError> {
        Ok(self.store.people_snapshot().await?.resolve(id).cloned())
    }

    /// Reads one stored source card.
    ///
    /// A [`Person`] carries [`PersonSourceId`]s, not cards, so this is how a host
    /// gets from a person it resolved to the provider record behind them — and the
    /// card is what the photo API takes, since a photo belongs to a source record
    /// rather than to the merged person.
    ///
    /// `None` when no synced source in `account` holds that contact.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when the store cannot be read, or
    /// [`ApiError::InvalidInput`] if the stored payload is not a card.
    pub async fn contact_card(
        &self,
        account: &AccountId,
        contact: &ContactId,
    ) -> Result<Option<ContactCard>, ApiError> {
        // Contact scopes only: a card is looked up by its provider key, which is not
        // unique across domains, and a mail message could otherwise answer.
        for scope in self.store.account_scopes(account.clone()).await? {
            if scope.object_kind() != Some(ObjectKind::ContactCard) {
                continue;
            }
            if let Some(payload) = self.store.object_payload(&scope, contact.key()).await? {
                return serde_json::from_value(payload)
                    .map(Some)
                    .map_err(|error| ApiError::InvalidInput(error.to_string()));
            }
        }
        Ok(None)
    }

    /// Resolves canonical email addresses to the people carrying them.
    ///
    /// Batched because the caller is a screenful of mail rows: a mail row names its
    /// sender by address, and resolving them one at a time is a store round-trip per
    /// row on every rebuild. Addresses nobody carries are absent from the map.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when the people index cannot be read.
    pub async fn people_by_email(
        &self,
        emails: &[CanonicalEmail],
    ) -> Result<BTreeMap<CanonicalEmail, Person>, ApiError> {
        Ok(self.store.people_by_email(emails).await?)
    }

    /// Returns one destination advertised by this source-bound adapter.
    pub fn contact_destination<P: ContactsProvider>(
        &self,
        provider: &P,
    ) -> Option<ContactDestination> {
        provider.contact_destination()
    }

    /// Returns the **writable** destinations advertised by `providers`, sorted and
    /// deduplicated by address book.
    ///
    /// This is the list a host offers as "save this contact to…", so a read-only
    /// source belongs in an address-book listing, not here. Each adapter is bound to
    /// one account already, which is why no account is taken: filtering by one here
    /// would only be re-stating what the caller chose when it assembled `providers`.
    pub fn contact_destinations<'a>(
        &self,
        providers: impl IntoIterator<Item = &'a dyn ContactsProvider>,
    ) -> Vec<ContactDestination> {
        let mut destinations: Vec<_> = providers
            .into_iter()
            .filter_map(ContactsProvider::contact_destination)
            .filter(|destination| destination.writable)
            .collect();
        destinations.sort_by(|left, right| left.address_book.cmp(&right.address_book));
        destinations.dedup_by(|left, right| left.address_book == right.address_book);
        destinations
    }

    /// Returns ranked global recipient suggestions and explicit coverage.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when people, observations, or coverage cannot be read.
    pub async fn recipient_suggestions(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<RecipientSuggestions, ApiError> {
        let people = self.store.people_snapshot().await?;
        let interactions = self.store.recipient_interactions(None).await?;
        Ok(RecipientSuggestions {
            suggestions: rank_recipient_suggestions(
                query,
                &people.people,
                &interactions,
                limit.min(200),
            ),
            coverage: self.store.recipient_coverage(None).await?,
        })
    }

    /// Suppresses current interaction history for one canonical email.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for an invalid email or
    /// [`ApiError::Store`] when suppression cannot be persisted.
    pub async fn forget_recipient(&self, email: &str) -> Result<usize, ApiError> {
        let email = engine_core::people::CanonicalEmail::parse(email)
            .map_err(|error| ApiError::InvalidInput(error.to_string()))?;
        Ok(self.store.forget_recipient(&email).await?)
    }

    /// Suppresses current interaction history for one account.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when suppression cannot be persisted.
    pub async fn clear_recipient_history(&self, account: &AccountId) -> Result<usize, ApiError> {
        Ok(self.store.clear_recipient_history(account.clone()).await?)
    }

    /// Suppresses all current interaction history.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when suppression cannot be persisted.
    pub async fn clear_all_recipient_history(&self) -> Result<usize, ApiError> {
        Ok(self.store.clear_all_recipient_history().await?)
    }

    /// Creates a contact through the outbox and refetches the canonical card.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for an unsupported destination/field,
    /// or the underlying outbox, store, or provider error.
    pub async fn create_contact<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        draft: &ContactDraft,
    ) -> Result<ContactWrite, ApiError> {
        validate_create(provider, draft)?;
        let write = create_contact(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            draft,
        )
        .await
        .map_err(map_sync_error)?;
        let reconciled = self
            .reconcile_contact(provider, account, &write.contact, false)
            .await;
        Ok(ContactWrite { write, reconciled })
    }

    /// Patches one explicit source card through the outbox.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for unsupported fields, or the
    /// underlying outbox, store, or provider error.
    #[allow(
        clippy::too_many_arguments,
        reason = "outbox idempotency and explicit source-card patch inputs"
    )]
    pub async fn patch_contact<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> Result<ContactWrite, ApiError> {
        validate_not_group(base)?;
        if matches!(
            patch.kind.as_ref(),
            Some(engine_core::contact::FieldPatch::Set(ContactKind::Group))
        ) {
            return Err(ApiError::InvalidInput(
                "contact group writes are not supported".into(),
            ));
        }
        validate_fields(provider, &patch.requested_fields())?;
        let write = patch_contact(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            base,
            patch,
        )
        .await
        .map_err(map_sync_error)?;
        let reconciled = self
            .reconcile_contact(provider, account, &write.contact, false)
            .await;
        Ok(ContactWrite { write, reconciled })
    }

    /// Deletes one explicit source card through the outbox.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for a read-only source, or the
    /// underlying outbox, store, or provider error.
    pub async fn delete_contact<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        base: &ContactCard,
    ) -> Result<ContactDelete, ApiError> {
        validate_not_group(base)?;
        validate_writable(provider)?;
        let op = delete_contact(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            base,
        )
        .await
        .map_err(map_sync_error)?;
        Ok(ContactDelete {
            op,
            reconciled: self
                .reconcile_contact(provider, account, &base.id, true)
                .await,
        })
    }

    async fn reconcile_contact<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
        contact: &engine_core::ids::ContactId,
        deleted: bool,
    ) -> ContactReconciled {
        let result = if deleted {
            reconcile_contact_deletion(provider, &self.store, account, contact, worker(), LEASE_TTL)
                .await
        } else {
            reconcile_contact_card(provider, &self.store, account, contact, worker(), LEASE_TTL)
                .await
        };
        match result.map_err(map_sync_error) {
            Ok(report) => ContactReconciled::Applied(report),
            Err(ApiError::Busy) => ContactReconciled::Busy,
            Err(error) => ContactReconciled::Failed(Box::new(error)),
        }
    }
}

fn validate_create<P: ContactsProvider>(
    provider: &P,
    draft: &ContactDraft,
) -> Result<(), ApiError> {
    validate_not_group(&draft.card)?;
    if !draft.card.members.is_empty() {
        return Err(ApiError::InvalidInput(
            "contact group membership writes are not supported".into(),
        ));
    }
    let destination = validate_writable(provider)?;
    if destination.address_book != draft.address_book {
        return Err(ApiError::InvalidInput(
            "contact draft targets a different address book".into(),
        ));
    }
    validate_fields(provider, &draft.requested_fields())
}

fn validate_not_group(card: &ContactCard) -> Result<(), ApiError> {
    if card.kind == ContactKind::Group {
        Err(ApiError::InvalidInput(
            "contact group writes are not supported".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_writable<P: ContactsProvider>(provider: &P) -> Result<ContactDestination, ApiError> {
    provider
        .contact_destination()
        .filter(|destination| destination.writable)
        .ok_or_else(|| ApiError::InvalidInput("contact source is not writable".into()))
}

fn validate_fields<P: ContactsProvider>(
    provider: &P,
    fields: &engine_core::contact::ContactFieldSet,
) -> Result<(), ApiError> {
    let destination = validate_writable(provider)?;
    if destination.supported_fields.contains_all(fields) {
        Ok(())
    } else {
        let unsupported: BTreeSet<_> = fields
            .iter()
            .filter(|field| !destination.supported_fields.contains(*field))
            .collect();
        Err(ApiError::InvalidInput(format!(
            "contact destination does not support fields {unsupported:?}"
        )))
    }
}
