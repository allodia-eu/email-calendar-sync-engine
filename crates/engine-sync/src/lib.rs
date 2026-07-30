//! `engine-sync` — sync orchestration.
//!
//! Step 4 ships the **thin per-scope loop** that drives one provider against one
//! store for one account's mail, exercising the full cycle the store contract
//! prescribes (`store-and-sync.md`):
//!
//! ```text
//! claim_sync_scope → provider fetch → project/derive → apply_sync_update → release
//! ```
//!
//! Per scope it claims the lease (getting the prior cursor), fetches the
//! normalized [`ScopeSync`], precomputes the
//! [`DerivedWrite`] with the pure `engine-core` projection *before* the store
//! call, commits the delta/snapshot atomically, and releases. A `StaleLease`
//! (the lease was superseded mid-flight — e.g. a suspended mobile worker resumed)
//! drops the lease, **re-claims with the fresh cursor, and recomputes**; it never
//! retries the stale write. Containers (mailboxes) sync before members (email),
//! the referential apply order the contract requires.
//!
//! The store owns tombstoning: a [`SyncUpdate::Snapshot`] tells it to remove local
//! rows (and their derived rows) absent from the present set; a delta removes the
//! listed keys. The loop only projects the *changed* objects.
//!
//! [`sync_mail_streamed`] is the responsive variant: it commits each email page as
//! it lands and reports [`SyncCommit`] to a [`SyncObserver`] for live "downloaded
//! Y of X" UI, advancing the cursor only on the final page.
//!
//! The full cross-scope orchestrator (dependency-ordered fan-out across many
//! scopes, the outbox workers, the tzdata-bump driver) is a later build step; this
//! is deliberately the minimal driver that proves the cycle end to end.

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Mailbox, Message},
    recipient::RecipientObservation,
    search_index::project_message,
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{Provider, ProviderError, ScopeSync};
use engine_store::{
    ApplyBatch, ContactStore, DerivedWrite, LeaseRequest, Store, StoreError, StoreRead,
    SyncApplied, WorkerId,
};

/// How many times a scope is re-claimed after a `StaleLease` before giving up.
pub(crate) const MAX_STALE_RECLAIMS: u32 = 3;

/// Why a sync or submission cycle failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The provider could not produce the changes (classified per
    /// [`FailureClass`](engine_core::error::FailureClass)).
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    /// The store rejected or could not commit the apply.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    /// An outbox bookkeeping failure (payload encoding, or a just-enqueued op that
    /// was not claimable).
    #[error("outbox error: {0}")]
    Outbox(String),
    /// A stored object payload could not be deserialized back into its domain type
    /// (a corrupt row, or a payload written by an incompatible schema).
    #[error("decode error: {0}")]
    Decode(String),
    /// A calendar reconcile was asked for on an event scope that has never been expanded,
    /// so there is no window to re-expand a changed event over.
    ///
    /// Only reachable by reconciling before the account's calendars have ever synced. It is
    /// an error rather than a silent no-op because expanding nothing would store the events
    /// with **zero** occurrence rows *and* advance the cursor — so the next sync would see
    /// no changes and the grid would stay empty forever. Sync the calendar first.
    #[error("no expansion window for this event scope: sync_calendar first")]
    NoExpansionWindow,
    /// Pure people derivation failed.
    #[error("people derivation error: {0}")]
    People(engine_core::people::PeopleError),
    /// Contact sources changed during every people-index CAS retry.
    #[error("contact sources kept changing during people-index rebuild")]
    ConcurrentPeopleRebuild,
}

impl SyncError {
    /// Wraps a stored payload's deserialization failure.
    pub(crate) fn decode(err: &serde_json::Error) -> Self {
        Self::Decode(err.to_string())
    }
}

/// What one `sync_mail` run applied, per scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailSyncReport {
    /// The mailbox-container apply result.
    pub mailboxes: SyncApplied,
    /// The email-member apply result.
    pub email: SyncApplied,
}

/// Syncs one account's mail: mailbox containers first, then email members.
///
/// Each scope runs the claim → fetch → derive → apply → release cycle with
/// `StaleLease` recovery. `worker` identifies the lease holder and `ttl` bounds it.
///
/// # Errors
///
/// Returns [`SyncError`] if the provider fetch fails or the store rejects the
/// apply for a reason other than a recoverable `StaleLease`.
pub async fn sync_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<MailSyncReport, SyncError>
where
    P: Provider,
    S: Store + StoreRead + ContactStore,
{
    let req = LeaseRequest::new(worker, ttl);
    let (mailboxes, ()) = run_scope(store, account, &MailboxScope(provider), &req)
        .await?
        .into_applied();
    let sent = recipients::sent_mailboxes(store, account).await?;
    if !sent.is_empty() {
        recipients::backfill(store, account, &sent).await?;
    }
    let (email, ()) = run_scope(store, account, &EmailScope(provider, &sent), &req)
        .await?
        .into_applied();
    recipients::record_coverage(
        store,
        account,
        provider.default_sync_window(),
        !sent.is_empty(),
    )
    .await?;
    Ok(MailSyncReport { mailboxes, email })
}

/// Syncs **only** one account's mailbox list (folder discovery) from `provider`,
/// skipping the email members.
///
/// The cross-folder orchestrator runs this **once per account**, then fans out the
/// per-folder email syncs ([`sync_email_streamed`]) concurrently. The folder-list
/// container is a single shared scope (`store-and-sync.md` referential apply order),
/// so syncing it once up front avoids the contention a repeated per-folder mailbox
/// sync would cause — letting the independent per-folder email scopes proceed in
/// parallel without fighting over it.
///
/// # Errors
///
/// Returns [`SyncError`] if the provider fetch fails or the store rejects the apply
/// for a reason other than a recoverable `StaleLease`.
pub async fn sync_mailbox_list<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<SyncApplied, SyncError>
where
    P: Provider,
    S: Store,
{
    let req = LeaseRequest::new(worker, ttl);
    Ok(run_scope(store, account, &MailboxScope(provider), &req)
        .await?
        .into_applied()
        .0)
}

/// What a fetch produced: something to apply, or a provider-side reason not to.
///
/// The `Halt` arm exists for a source that answered "not available right now"
/// (a CardDAV address book the server stopped serving) rather than failing. The
/// driver releases the lease and hands the reason back; any bookkeeping that
/// implies belongs to the caller, not to the shared loop.
pub(crate) enum ScopeFetch<T, M, H> {
    /// Apply this batch, carrying `meta` through to the caller.
    Proceed { sync: ScopeSync<T>, meta: M },
    /// Do not apply; return `H` to the caller.
    Halt(H),
}

/// The outcome of [`run_scope`].
pub(crate) enum ScopeRun<M, H> {
    /// The batch was applied under a valid lease.
    Applied { applied: SyncApplied, meta: M },
    /// The fetch declined to produce a batch.
    Halted(H),
}

impl<M> ScopeRun<M, core::convert::Infallible> {
    /// Unwraps a run whose syncer cannot halt (`Halt = Infallible`), so the mail
    /// scopes do not carry a branch that is uninhabited by construction.
    pub(crate) fn into_applied(self) -> (SyncApplied, M) {
        match self {
            Self::Applied { applied, meta } => (applied, meta),
            Self::Halted(never) => match never {},
        }
    }
}

/// A scope-typed fetch + projection, so [`run_scope`] holds the lease/retry logic
/// once and the per-type difference (which provider method, which projection, which
/// extra rows ride the same transaction) is supplied by an impl.
///
/// Every scope — mailboxes, email, address books, contact cards — goes through this
/// one driver. The claim/fence/reclaim/release discipline is the part that is easy to
/// get subtly wrong, so it must not be copied per scope type.
#[async_trait::async_trait]
pub(crate) trait ScopeSyncer: Sync {
    /// The normalized object type stored under this scope.
    type Object: engine_store::StorableObject + serde::Serialize + Send + Sync;

    /// Extra per-fetch information carried through to the caller (contact sync uses
    /// it for "the cursor was rebuilt"). `()` when there is none.
    type Meta: Send;

    /// What a non-applying fetch returns. [`core::convert::Infallible`] when the
    /// fetch either applies or errors.
    type Halt: Send;

    /// The scope this syncer claims and applies under.
    fn scope(&self, account: &AccountId) -> SyncScope;

    /// Fetches the scope's changes since `cursor`.
    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<Self::Object, Self::Meta, Self::Halt>, ProviderError>;

    /// Precomputes the derived (FTS/structured) rows for the changed objects.
    fn derive(&self, update: &SyncUpdate<Self::Object>) -> DerivedWrite;

    /// Recipient observations to write in the *same* transaction as the batch.
    /// Empty for every scope but email.
    fn observations(
        &self,
        _account: &AccountId,
        _update: &SyncUpdate<Self::Object>,
    ) -> Vec<RecipientObservation> {
        Vec::new()
    }
}

/// Runs the claim → fetch → derive → apply → release cycle for one scope, with
/// `StaleLease` re-claim-and-recompute.
///
/// This is the single place the lease discipline lives: the lease is released on
/// every exit path (including a provider fetch failure, so a leaked lease does not
/// block the next sync with a spurious `ScopeHeld`), and a `StaleLease` write is
/// never retried — the loop re-claims and recomputes from the fresh cursor instead.
pub(crate) async fn run_scope<S, Y>(
    store: &S,
    account: &AccountId,
    syncer: &Y,
    req: &LeaseRequest,
) -> Result<ScopeRun<Y::Meta, Y::Halt>, SyncError>
where
    S: Store,
    Y: ScopeSyncer,
{
    let scope = syncer.scope(account);
    let mut reclaims = 0u32;
    loop {
        let claim = store
            .claim_sync_scope(account.clone(), &scope, req.clone())
            .await?;
        // Release the lease if the provider fetch fails (e.g. the account is offline),
        // so a leaked lease does not block the next sync with a spurious `ScopeHeld`.
        let fetched = match syncer.fetch(account, claim.state.as_ref()).await {
            Ok(fetched) => fetched,
            Err(err) => {
                let _ = store.release_sync_scope(claim.lease).await;
                return Err(err.into());
            }
        };
        let (sync, meta) = match fetched {
            ScopeFetch::Proceed { sync, meta } => (sync, meta),
            ScopeFetch::Halt(halt) => {
                store.release_sync_scope(claim.lease).await?;
                return Ok(ScopeRun::Halted(halt));
            }
        };
        let derived = syncer.derive(&sync.update);
        let observations = syncer.observations(account, &sync.update);
        let batch = ApplyBatch::new(&sync.update, &derived, &[], &sync.next_cursor)
            .with_recipient_observations(&observations);
        match store.apply_sync_update(&claim.lease, batch).await {
            Ok(applied) => {
                store.release_sync_scope(claim.lease).await?;
                return Ok(ScopeRun::Applied { applied, meta });
            }
            Err(StoreError::StaleLease) if reclaims < MAX_STALE_RECLAIMS => {
                // The lease was superseded after we read the cursor. Drop it and
                // start over with a fresh claim — never retry the stale write.
                reclaims += 1;
            }
            Err(other) => {
                // Best-effort release so a held lease does not block the next sync.
                let _ = store.release_sync_scope(claim.lease).await;
                return Err(other.into());
            }
        }
    }
}

/// The mailbox-container scope syncer.
pub(crate) struct MailboxScope<'p, P>(pub(crate) &'p P);

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for MailboxScope<'_, P> {
    type Halt = core::convert::Infallible;
    type Meta = ();
    type Object = Mailbox;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.mailbox_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<Mailbox, (), Self::Halt>, ProviderError> {
        self.0
            .sync_mailboxes(account, cursor)
            .await
            .map(|sync| ScopeFetch::Proceed { sync, meta: () })
    }

    fn derive(&self, _update: &SyncUpdate<Mailbox>) -> DerivedWrite {
        // Containers carry no full-text/structured index rows; only their object
        // payload (name, role, hierarchy) is stored.
        DerivedWrite::empty()
    }
}

/// The email-member scope syncer.
///
/// Carries the account's Sent mailboxes so the recipient observations they imply are
/// written in the *same* transaction as the batch — via the shared driver's
/// `observations` hook rather than a copy of the whole lease loop.
struct EmailScope<'p, P>(&'p P, &'p BTreeSet<MailboxId>);

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for EmailScope<'_, P> {
    type Halt = core::convert::Infallible;
    type Meta = ();
    type Object = Message;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.email_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<Message, (), Self::Halt>, ProviderError> {
        self.0
            .sync_email(account, cursor)
            .await
            .map(|sync| ScopeFetch::Proceed { sync, meta: () })
    }

    fn derive(&self, update: &SyncUpdate<Message>) -> DerivedWrite {
        derive_messages(changed_objects(update))
    }

    fn observations(
        &self,
        account: &AccountId,
        update: &SyncUpdate<Message>,
    ) -> Vec<RecipientObservation> {
        recipients::observations(account, update, self.1)
    }
}

/// Projects messages into their derived (full-text/structured/membership) rows —
/// shared by the whole-scope [`EmailScope`] and the streaming email loop.
pub(crate) fn derive_messages(messages: &[Message]) -> DerivedWrite {
    let mut derived = DerivedWrite::empty();
    for message in messages {
        derived.push_mail(project_message(message));
    }
    derived
}

/// The created-or-updated objects an update carries (a delta's `changed` or a
/// snapshot's `objects`) — what gets projected. Tombstoned/removed keys are the
/// store's job, not the projection's.
pub(crate) fn changed_objects<T>(update: &SyncUpdate<T>) -> &[T] {
    match update {
        SyncUpdate::Delta { changed, .. } => changed,
        SyncUpdate::Snapshot { objects, .. } => objects,
    }
}

mod attachment;
mod body;
mod calendar;
mod contact;
mod horizon;
mod observer;
mod outbox;
mod progress;
mod recipients;
mod stream;
mod threading;
pub use attachment::{
    fetch_message_attachment, fetch_message_attachments, fetch_message_scheduling,
};
pub use body::{fetch_inline_parts, fetch_message_body};
pub use calendar::{CalendarSyncReport, EventSyncReport, reconcile_calendar_events, sync_calendar};
pub use contact::{
    ContactReconcileReport, ContactSourceReport, ContactSyncReport, PeopleRebuildReport,
    rebuild_people_index, reconcile_contact_card, reconcile_contact_deletion, sync_address_books,
    sync_contact_cards, sync_contacts,
};
pub use horizon::{HorizonExpansion, UnexpandableEvent, expand_calendar_horizon};
pub use observer::{IgnoreCommits, SyncCommit, SyncObserver};
pub use outbox::{
    CalendarWriteOutcome, ContactWriteOutcome, MailEditOutcome, SubmitOutcome,
    create_calendar_event, create_contact, delete_calendar_event, delete_contact, edit_mail,
    patch_calendar_event, patch_contact, put_calendar_document, submit_mail,
};
pub use progress::{AccountProgress, ProgressSnapshot};
pub use stream::{StreamTuning, sync_email_streamed, sync_mail_streamed};
pub use threading::{ThreadDeriveReport, derive_mail_threads};

#[cfg(test)]
mod tests;
