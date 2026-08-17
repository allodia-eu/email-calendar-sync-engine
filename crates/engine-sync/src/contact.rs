//! Contact container/card orchestration and generation-CAS people rebuilds.

use core::time::Duration;

use async_trait::async_trait;
use engine_core::{
    contact::{AddressBook, ContactCard},
    ids::{AccountId, ContactId},
    people::{PeopleError, rebuild_people},
    sync::{SyncObject, SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    ContactSourceSync, ContactUnavailable, ContactsProvider, ProviderError, ScopeSync,
};
use engine_store::{
    ApplyBatch, ContactSourceAvailability, ContactStore, DerivedWrite, LeaseRequest, Store,
    StoreRead, SyncApplied, WorkerId,
};

use crate::{ScopeFetch, ScopeRun, ScopeSyncer, SyncError, run_scope};

/// Maximum rebuild retries after a concurrent contact apply wins the CAS race.
const MAX_PEOPLE_REBUILDS: u32 = 3;

/// Result for one independently available contact source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSourceReport {
    /// Apply counts; zero when unavailable.
    pub applied: SyncApplied,
    /// Stable reason when this optional source was unavailable.
    pub unavailable: Option<String>,
    /// Whether an invalid/expired cursor forced snapshot recovery.
    pub cursor_recovered: bool,
}

/// Result of a generation-CAS people rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeopleRebuildReport {
    /// Contact-source generation materialized.
    pub generation: u64,
    /// Number of people produced.
    pub people: usize,
    /// Number of retired id aliases retained.
    pub aliases: usize,
    /// CAS races retried.
    pub retries: u32,
}

/// Combined discovery, card sync, and people materialization report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSyncReport {
    /// Address-book/source discovery.
    pub address_books: ContactSourceReport,
    /// Cards for this source-bound adapter.
    pub cards: ContactSourceReport,
    /// People generation after the card apply.
    pub people: PeopleRebuildReport,
}

/// Direct post-write card reconciliation without moving the normal sync cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactReconcileReport {
    /// Source-object apply counts.
    pub applied: SyncApplied,
    /// Rebuilt people generation.
    pub people: PeopleRebuildReport,
}

/// Syncs address-book/source discovery once for an account.
///
/// # Errors
///
/// Returns [`SyncError`] for provider or store failures. Independently
/// unavailable sources are successful reports, not errors.
pub async fn sync_address_books<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ContactSourceReport, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead + ContactStore,
{
    run_contact_scope(
        store,
        account,
        &AddressBookScope(provider),
        &LeaseRequest::new(worker, ttl),
    )
    .await
}

/// Syncs cards for one source-bound adapter and rebuilds people.
///
/// # Errors
///
/// Returns [`SyncError`] for provider/store failures or repeated people-CAS
/// races.
pub async fn sync_contact_cards<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<(ContactSourceReport, PeopleRebuildReport), SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead + ContactStore,
{
    let cards = run_contact_scope(
        store,
        account,
        &CardScope(provider),
        &LeaseRequest::new(worker, ttl),
    )
    .await?;
    let people = rebuild_people_index(store).await?;
    Ok((cards, people))
}

/// Convenience combined sync for account-global adapters.
///
/// # Errors
///
/// Returns [`SyncError`] under the same rules as the two source-level methods.
pub async fn sync_contacts<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ContactSyncReport, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead + ContactStore,
{
    let request = LeaseRequest::new(worker, ttl);
    let address_books =
        run_contact_scope(store, account, &AddressBookScope(provider), &request).await?;
    let cards = run_contact_scope(store, account, &CardScope(provider), &request).await?;
    let people = rebuild_people_index(store).await?;
    Ok(ContactSyncReport {
        address_books,
        cards,
        people,
    })
}

/// Rebuilds people from a consistent contact generation, retrying CAS races.
///
/// # Errors
///
/// Returns [`SyncError::People`] for derivation failure or
/// [`SyncError::ConcurrentPeopleRebuild`] after repeated contact changes.
pub async fn rebuild_people_index<S>(store: &S) -> Result<PeopleRebuildReport, SyncError>
where
    S: ContactStore,
{
    for retries in 0..=MAX_PEOPLE_REBUILDS {
        let sources = store.contact_sources().await?;
        let previous = store.people_snapshot().await?;
        let people = rebuild_people(&sources.sources, &previous)?;
        let report = PeopleRebuildReport {
            generation: sources.generation,
            people: people.people.len(),
            aliases: people.aliases.len(),
            retries,
        };
        if store.replace_people(sources.generation, &people).await? {
            return Ok(report);
        }
    }
    Err(SyncError::ConcurrentPeopleRebuild)
}

/// Refetches and applies one server-canonical card while retaining the normal
/// source cursor.
///
/// # Errors
///
/// Returns [`SyncError`] for provider fetch, fenced apply, or people rebuild failure.
pub async fn reconcile_contact_card<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    contact: &ContactId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ContactReconcileReport, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead + ContactStore,
{
    let card = provider.fetch_contact(account, contact).await?;
    reconcile_contact_update(
        provider,
        store,
        account,
        SyncUpdate::delta(vec![card], Vec::new()),
        worker,
        ttl,
    )
    .await
}

/// Tombstones one written deletion while retaining the normal source cursor.
///
/// # Errors
///
/// Returns [`SyncError`] for fenced apply or people rebuild failure.
pub async fn reconcile_contact_deletion<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    contact: &ContactId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ContactReconcileReport, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead + ContactStore,
{
    reconcile_contact_update(
        provider,
        store,
        account,
        SyncUpdate::delta(Vec::<ContactCard>::new(), vec![contact.key().clone()]),
        worker,
        ttl,
    )
    .await
}

async fn reconcile_contact_update<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    update: SyncUpdate<ContactCard>,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ContactReconcileReport, SyncError>
where
    P: ContactsProvider,
    S: Store + StoreRead + ContactStore,
{
    let scope = provider.contact_scope(account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, LeaseRequest::new(worker, ttl))
        .await?;
    let Some(cursor) = claim.state.as_ref() else {
        store.release_sync_scope(claim.lease).await?;
        return Err(SyncError::Outbox(
            "contact source must be synced before a write can reconcile".into(),
        ));
    };
    let derived = DerivedWrite::empty();
    let batch = ApplyBatch::new(&update, &derived, &[], cursor);
    let applied = match store.apply_sync_update(&claim.lease, batch).await {
        Ok(applied) => applied,
        Err(error) => {
            let _ = store.release_sync_scope(claim.lease).await;
            return Err(error.into());
        }
    };
    store.release_sync_scope(claim.lease).await?;
    Ok(ContactReconcileReport {
        applied,
        people: rebuild_people_index(store).await?,
    })
}

struct AddressBookScope<'a, P>(&'a P);

#[async_trait]
impl<P: ContactsProvider> ScopeSyncer for AddressBookScope<'_, P> {
    type Halt = ContactUnavailable;
    type Meta = bool;
    type Object = AddressBook;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.address_book_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<AddressBook, bool, ContactUnavailable>, ProviderError> {
        self.0
            .sync_address_books(account, cursor)
            .await
            .map(contact_fetch)
    }

    fn derive(&self, _sync: &ScopeSync<AddressBook>) -> DerivedWrite {
        DerivedWrite::empty()
    }
}

struct CardScope<'a, P>(&'a P);

#[async_trait]
impl<P: ContactsProvider> ScopeSyncer for CardScope<'_, P> {
    type Halt = ContactUnavailable;
    type Meta = bool;
    type Object = ContactCard;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.contact_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<ContactCard, bool, ContactUnavailable>, ProviderError> {
        self.0
            .sync_contacts(account, cursor)
            .await
            .map(contact_fetch)
    }

    fn derive(&self, _sync: &ScopeSync<ContactCard>) -> DerivedWrite {
        DerivedWrite::empty()
    }
}

/// Maps a contact source's availability answer onto the shared driver's fetch
/// outcome: an unavailable source halts instead of applying an empty batch.
fn contact_fetch<T: SyncObject>(
    sync: ContactSourceSync<T>,
) -> ScopeFetch<T, bool, ContactUnavailable> {
    match sync {
        ContactSourceSync::Available {
            sync,
            cursor_recovered,
        } => ScopeFetch::Proceed {
            sync,
            meta: cursor_recovered,
        },
        ContactSourceSync::Unavailable(unavailable) => ScopeFetch::Halt(unavailable),
    }
}

/// Runs one contact scope through the shared [`run_scope`] driver, then records the
/// source's availability.
///
/// The lease/fence/reclaim discipline lives in `run_scope`; everything here is the
/// contact-specific bookkeeping the driver deliberately does not know about.
async fn run_contact_scope<S, Y>(
    store: &S,
    account: &AccountId,
    syncer: &Y,
    request: &LeaseRequest,
) -> Result<ContactSourceReport, SyncError>
where
    S: Store + StoreRead + ContactStore,
    Y: ScopeSyncer<Meta = bool, Halt = ContactUnavailable>,
{
    let scope = syncer.scope(account);
    match run_scope(store, account, syncer, request).await? {
        ScopeRun::Applied {
            applied,
            meta: cursor_recovered,
        } => {
            store
                .set_contact_source_availability(&scope, &ContactSourceAvailability::Available)
                .await?;
            Ok(ContactSourceReport {
                applied,
                unavailable: None,
                cursor_recovered,
            })
        }
        ScopeRun::Halted(unavailable) => {
            store
                .set_contact_source_availability(
                    &scope,
                    &ContactSourceAvailability::Unavailable {
                        reason: unavailable.reason.clone(),
                    },
                )
                .await?;
            Ok(ContactSourceReport {
                applied: SyncApplied::default(),
                unavailable: Some(unavailable.reason),
                cursor_recovered: false,
            })
        }
    }
}

impl From<PeopleError> for SyncError {
    fn from(error: PeopleError) -> Self {
        Self::People(error)
    }
}
