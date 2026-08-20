//! In-memory contact/people/recipient derived-store operations.

use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use engine_core::{
    contact::ContactCard,
    ids::{AccountId, ContactId},
    people::{CanonicalEmail, PeopleSnapshot, Person, PersonSource},
    recipient::{RecipientCoverage, RecipientInteraction, RecipientObservation},
    sync::{ObjectKind, SyncScope},
};

use super::{MemStore, PhotoCell};
use crate::{
    CachedContactPhoto, Clock, ContactPhotoCache, ContactSourceAvailability, ContactSourceSnapshot,
    ContactStore, Result, StoreError,
};

#[async_trait]
impl<C: Clock> ContactStore for MemStore<C> {
    async fn contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
        negative_ttl: Duration,
    ) -> Result<ContactPhotoCache> {
        let now = self.clock.now();
        let inner = self.lock();
        let Some(cell) = inner
            .contact_photos
            .get(&(account.clone(), contact.clone(), resource.to_owned()))
            .filter(|cell| cell.fingerprint == fingerprint)
        else {
            return Ok(ContactPhotoCache::Miss);
        };
        Ok(match &cell.photo {
            Some(photo) => ContactPhotoCache::Hit(photo.clone()),
            None if cell
                .fetched_at
                .checked_add(negative_ttl)
                .is_some_and(|expiry| expiry > now) =>
            {
                ContactPhotoCache::NoPhoto
            }
            None => ContactPhotoCache::Miss,
        })
    }

    async fn put_contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        photo: &CachedContactPhoto,
    ) -> Result<()> {
        let fetched_at = self.clock.now();
        self.lock().contact_photos.insert(
            (account.clone(), contact.clone(), resource.to_owned()),
            PhotoCell {
                fingerprint: photo.fingerprint.clone(),
                photo: Some(photo.clone()),
                fetched_at,
            },
        );
        Ok(())
    }

    async fn put_contact_photo_absent(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let fetched_at = self.clock.now();
        self.lock().contact_photos.insert(
            (account.clone(), contact.clone(), resource.to_owned()),
            PhotoCell {
                photo: None,
                fingerprint: fingerprint.to_owned(),
                fetched_at,
            },
        );
        Ok(())
    }

    async fn people_by_email(
        &self,
        emails: &[CanonicalEmail],
    ) -> Result<BTreeMap<CanonicalEmail, Person>> {
        let wanted: BTreeSet<&CanonicalEmail> = emails.iter().collect();
        let inner = self.lock();
        let mut found = BTreeMap::new();
        for person in &inner.people.people {
            for email in &person.emails {
                if wanted.contains(&email.value) {
                    found
                        .entry(email.value.clone())
                        .or_insert_with(|| person.clone());
                }
            }
        }
        Ok(found)
    }

    async fn contact_sources(&self) -> Result<ContactSourceSnapshot> {
        let inner = self.lock();
        let mut sources = Vec::new();
        for (scope, cell) in &inner.scopes {
            if scope.object_kind() != Some(ObjectKind::ContactCard) {
                continue;
            }
            for value in cell.objects.values() {
                let card: ContactCard = serde_json::from_value(value.clone())
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                sources.push(PersonSource::new(
                    scope.account().clone(),
                    card.clone(),
                    card.source_class,
                    card.is_writable,
                ));
            }
        }
        sources.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(ContactSourceSnapshot {
            generation: inner.contact_generation,
            sources,
        })
    }

    async fn people_snapshot(&self) -> Result<PeopleSnapshot> {
        Ok(self.lock().people.clone())
    }

    async fn replace_people(
        &self,
        expected_generation: u64,
        people: &PeopleSnapshot,
    ) -> Result<bool> {
        let mut inner = self.lock();
        if inner.contact_generation != expected_generation {
            return Ok(false);
        }
        inner.people = people.clone();
        Ok(true)
    }

    async fn recipient_interactions(
        &self,
        account: Option<AccountId>,
    ) -> Result<Vec<RecipientInteraction>> {
        let inner = self.lock();
        let mut grouped: BTreeMap<CanonicalEmail, InteractionAccumulator> = BTreeMap::new();
        for ((row_account, _, email), cell) in &inner.observations {
            if cell.suppressed || account.as_ref().is_some_and(|filter| filter != row_account) {
                continue;
            }
            grouped
                .entry(email.clone())
                .or_default()
                .observe(&cell.observation);
        }
        Ok(grouped
            .into_iter()
            .map(|(email, value)| value.finish(email))
            .collect())
    }

    async fn forget_recipient(&self, email: &CanonicalEmail) -> Result<usize> {
        Ok(suppress(&mut self.lock(), |_, row_email| {
            row_email == email
        }))
    }

    async fn clear_recipient_history(&self, account: AccountId) -> Result<usize> {
        Ok(suppress(&mut self.lock(), |row_account, _| {
            row_account == &account
        }))
    }

    async fn clear_all_recipient_history(&self) -> Result<usize> {
        Ok(suppress(&mut self.lock(), |_, _| true))
    }

    async fn recipient_index_version(&self, account: &AccountId) -> Result<Option<u32>> {
        Ok(self.lock().recipient_versions.get(account).copied())
    }

    async fn apply_recipient_backfill(
        &self,
        account: AccountId,
        version: u32,
        observations: &[RecipientObservation],
    ) -> Result<bool> {
        let mut inner = self.lock();
        if inner
            .recipient_versions
            .get(&account)
            .is_some_and(|current| *current >= version)
        {
            return Ok(false);
        }
        for observation in observations {
            let key = (
                observation.account.clone(),
                observation.source_message.clone(),
                observation.email.clone(),
            );
            inner
                .observations
                .entry(key)
                .or_insert_with(|| super::ObservationCell {
                    observation: observation.clone(),
                    suppressed: false,
                });
        }
        inner.recipient_versions.insert(account, version);
        Ok(true)
    }

    async fn set_recipient_coverage(&self, coverage: &RecipientCoverage) -> Result<()> {
        self.lock()
            .recipient_coverage
            .insert(coverage.account.clone(), coverage.clone());
        Ok(())
    }

    async fn recipient_coverage(
        &self,
        account: Option<AccountId>,
    ) -> Result<Vec<RecipientCoverage>> {
        let inner = self.lock();
        Ok(match account {
            Some(account) => inner
                .recipient_coverage
                .get(&account)
                .cloned()
                .into_iter()
                .collect(),
            None => inner.recipient_coverage.values().cloned().collect(),
        })
    }

    async fn set_contact_source_availability(
        &self,
        scope: &SyncScope,
        availability: &ContactSourceAvailability,
    ) -> Result<()> {
        self.lock()
            .contact_availability
            .insert(scope.clone(), availability.clone());
        Ok(())
    }

    async fn contact_source_availability(
        &self,
        account: AccountId,
    ) -> Result<Vec<(SyncScope, ContactSourceAvailability)>> {
        Ok(self
            .lock()
            .contact_availability
            .iter()
            .filter(|(scope, _)| scope.account() == &account)
            .map(|(scope, availability)| (scope.clone(), availability.clone()))
            .collect())
    }
}

#[derive(Default)]
struct InteractionAccumulator {
    names: BTreeSet<String>,
    count: u64,
    last_sent: Option<engine_core::time::UtcDateTime>,
}

impl InteractionAccumulator {
    fn observe(&mut self, observation: &engine_core::recipient::RecipientObservation) {
        if let Some(name) = observation
            .name
            .as_ref()
            .filter(|name| !name.trim().is_empty())
        {
            self.names.insert(name.clone());
        }
        self.count = self.count.saturating_add(1);
        self.last_sent = self.last_sent.max(observation.sent_at);
    }

    fn finish(self, email: CanonicalEmail) -> RecipientInteraction {
        RecipientInteraction::new(
            email,
            self.names.into_iter().next(),
            self.count,
            self.last_sent,
        )
    }
}

fn suppress(
    inner: &mut super::Inner,
    matches: impl Fn(&AccountId, &CanonicalEmail) -> bool,
) -> usize {
    let mut changed = 0;
    for ((account, _, email), cell) in &mut inner.observations {
        if !cell.suppressed && matches(account, email) {
            cell.suppressed = true;
            changed += 1;
        }
    }
    changed
}
