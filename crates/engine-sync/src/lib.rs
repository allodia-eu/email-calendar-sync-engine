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

use engine_core::{
    ids::AccountId,
    mail::{Mailbox, Message},
    search_index::project_message,
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{Provider, ProviderError, ScopeSync};
use engine_store::{
    ApplyBatch, DerivedWrite, LeaseRequest, Store, StoreError, SyncApplied, WorkerId,
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
    S: Store,
{
    let req = LeaseRequest::new(worker, ttl);
    let mailboxes = run_scope(store, account, &MailboxScope(provider), &req).await?;
    let email = run_scope(store, account, &EmailScope(provider), &req).await?;
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
    run_scope(store, account, &MailboxScope(provider), &req).await
}

/// A scope-typed fetch + projection, so [`run_scope`] holds the lease/retry logic
/// once and the per-type difference (which provider method, which projection) is
/// supplied by an impl.
#[async_trait::async_trait]
pub(crate) trait ScopeSyncer: Sync {
    /// The normalized object type stored under this scope.
    type Object: engine_store::StorableObject + serde::Serialize + Send + Sync;

    /// The scope this syncer claims and applies under.
    fn scope(&self, account: &AccountId) -> SyncScope;

    /// Fetches the scope's changes since `cursor`.
    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeSync<Self::Object>, ProviderError>;

    /// Precomputes the derived (FTS/structured) rows for the changed objects.
    fn derive(&self, update: &SyncUpdate<Self::Object>) -> DerivedWrite;
}

/// Runs the claim → fetch → derive → apply → release cycle for one scope, with
/// `StaleLease` re-claim-and-recompute.
pub(crate) async fn run_scope<S, Y>(
    store: &S,
    account: &AccountId,
    syncer: &Y,
    req: &LeaseRequest,
) -> Result<SyncApplied, SyncError>
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
        let derived = syncer.derive(&fetched.update);
        let batch = ApplyBatch::new(&fetched.update, &derived, &[], &fetched.next_cursor);
        match store.apply_sync_update(&claim.lease, batch).await {
            Ok(applied) => {
                store.release_sync_scope(claim.lease).await?;
                return Ok(applied);
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
    type Object = Mailbox;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.mailbox_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeSync<Mailbox>, ProviderError> {
        self.0.sync_mailboxes(account, cursor).await
    }

    fn derive(&self, _update: &SyncUpdate<Mailbox>) -> DerivedWrite {
        // Containers carry no full-text/structured index rows; only their object
        // payload (name, role, hierarchy) is stored.
        DerivedWrite::empty()
    }
}

/// The email-member scope syncer.
struct EmailScope<'p, P>(&'p P);

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for EmailScope<'_, P> {
    type Object = Message;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.email_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeSync<Message>, ProviderError> {
        self.0.sync_email(account, cursor).await
    }

    fn derive(&self, update: &SyncUpdate<Message>) -> DerivedWrite {
        derive_messages(changed_objects(update))
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
mod horizon;
mod observer;
mod outbox;
mod progress;
mod stream;
mod threading;
pub use attachment::{fetch_message_attachment, fetch_message_attachments};
pub use body::{fetch_inline_parts, fetch_message_body};
pub use calendar::{CalendarSyncReport, EventSyncReport, reconcile_calendar_events, sync_calendar};
pub use horizon::{HorizonExpansion, UnexpandableEvent, expand_calendar_horizon};
pub use observer::{IgnoreCommits, SyncCommit, SyncObserver};
pub use outbox::{
    CalendarWriteOutcome, MailEditOutcome, SubmitOutcome, create_calendar_event,
    delete_calendar_event, edit_mail, patch_calendar_event, put_calendar_document, submit_mail,
};
pub use progress::{AccountProgress, ProgressSnapshot};
pub use stream::{StreamTuning, sync_email_streamed, sync_mail_streamed};
pub use threading::{ThreadDeriveReport, derive_mail_threads};

#[cfg(test)]
mod tests;
