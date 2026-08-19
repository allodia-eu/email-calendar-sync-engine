//! Streaming mail sync: commit each email chunk as it lands, report the exact rows
//! that changed, and **checkpoint the cursor mid-pass** so a killed sync resumes.
//!
//! The responsive counterpart to [`crate::sync_mail`]. The whole-scope loop in
//! `lib.rs` claims, fetches, and applies one scope atomically; here the email scope
//! is driven chunk by chunk under a single lease (from [`Provider::stream_email`])
//! so a host UI renders recent mail and live "downloaded Y of X" feedback before the
//! sync finishes.
//!
//! Two knobs, decoupled (`store-and-sync.md`): the [`StreamTuning::fetch_batch`]
//! bounds each network round trip; the [`StreamTuning::chunk_size`] bounds how often
//! the loop commits and reports. A large batch with a small chunk gives few round
//! trips *and* row-as-it-arrives commits.
//!
//! Resumability follows each chunk's [`advance_to`](engine_provider::EmailChunk):
//! an additive pass (cold backfill or delta) advances the cursor on every chunk, so
//! a crash resumes from the last checkpoint instead of re-downloading from the
//! start; a reconciling pass holds the cursor until its final chunk tombstones
//! (rare, restarts on crash).

use std::{collections::BTreeSet, time::Instant};

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::Message,
    search_index::project_state_change,
    sync::{SyncState, SyncUpdate, SyncWindow},
};
use engine_provider::{EmailChunk, PassMode, Provider};
use engine_store::{ApplyBatch, LeaseRequest, Store, StoreError, StoreRead, SyncApplied};
use futures_util::StreamExt;

use crate::{
    MAX_STALE_RECLAIMS, SyncCommit, SyncError, SyncObserver, derive_messages,
    mail_account::SyncTiming, recipients,
};

/// How a streaming sync runs: the depth window, plus how it separates network
/// batching from commit granularity.
///
/// - [`window`](Self::window): the sync-depth bound (a snapshot/backfill fetches only mail on or
///   after it); the full history by default.
/// - [`fetch_batch`](Self::fetch_batch): objects per provider round trip (`0` = the provider's
///   protocol maximum). Larger = fewer round trips.
/// - [`chunk_size`](Self::chunk_size): objects committed and reported per chunk (`0` = one chunk
///   per batch). Smaller = rows appear sooner.
///
/// A large `fetch_batch` with a small `chunk_size` is the sweet spot: few round
/// trips *and* immediate row-by-row rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTuning {
    /// The sync-depth window (full history by default).
    pub window: SyncWindow,
    /// Objects per network round trip (`0` = provider maximum).
    pub fetch_batch: usize,
    /// Objects committed and reported per chunk (`0` = one chunk per batch).
    pub chunk_size: usize,
}

impl StreamTuning {
    /// A tuning with an explicit batch and chunk size, over the full history.
    #[must_use]
    pub fn new(fetch_batch: usize, chunk_size: usize) -> Self {
        Self {
            window: SyncWindow::full(),
            fetch_batch,
            chunk_size,
        }
    }

    /// The responsive default: a large batch (few round trips) with a small chunk
    /// (rows surface almost immediately) — what an interactive mail client wants.
    #[must_use]
    pub fn responsive() -> Self {
        Self {
            window: SyncWindow::full(),
            fetch_batch: 200,
            chunk_size: 3,
        }
    }

    /// A throughput-biased tuning: a large batch committed in larger chunks — for a
    /// background backfill where per-row latency does not matter.
    #[must_use]
    pub fn bulk() -> Self {
        Self {
            window: SyncWindow::full(),
            fetch_batch: 500,
            chunk_size: 100,
        }
    }

    /// Bounds this tuning to a sync-depth `window` (builder-style).
    #[must_use]
    pub fn within(mut self, window: SyncWindow) -> Self {
        self.window = window;
        self
    }
}

impl Default for StreamTuning {
    fn default() -> Self {
        Self::responsive()
    }
}

/// Streams the email scope chunk by chunk under one lease. Each additive chunk
/// commits and advances the cursor to its checkpoint (resumable); a reconciling pass
/// holds the cursor and tombstones against the accumulated present set on its final
/// chunk. A `StaleLease` abandons the partial stream and restarts from a fresh claim;
/// the checkpointed (additive) or held (reconcile) cursor makes that safe, and the
/// adapter reconciles its own transport on the next fetch.
pub(crate) struct FolderPass<'a, S, O> {
    /// Where the chunks land.
    pub(crate) store: &'a S,
    /// The lease every scope in this pass claims under.
    pub(crate) req: &'a LeaseRequest,
    /// Depth window, fetch batching and commit granularity.
    pub(crate) tuning: StreamTuning,
    /// Told about every committed chunk.
    pub(crate) observer: &'a O,
    /// The account's Sent mailboxes, resolved once for the whole pass.
    pub(crate) sent: &'a BTreeSet<MailboxId>,
}

/// Streams one folder's mail into the store, filling `timing` as it goes.
///
/// Everything that is the same for every folder of a pass travels in [`FolderPass`], so what
/// varies per call is the folder's provider, the account, and where its measurements go.
pub(crate) async fn stream_email<P, S, O>(
    provider: &P,
    account: &AccountId,
    pass: &FolderPass<'_, S, O>,
    timing: &mut SyncTiming,
) -> Result<SyncApplied, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
    O: SyncObserver,
{
    let FolderPass {
        store,
        req,
        tuning,
        observer,
        sent,
    } = *pass;
    let scope = provider.email_scope(account);
    let mut reclaims = 0u32;
    'restart: loop {
        let claim = store
            .claim_sync_scope(account.clone(), &scope, req.clone())
            .await?;
        let lease = claim.lease;
        // The cursor each pass resumes the *provider* fetch from; the store advances
        // it per additive checkpoint, or only on the final reconcile chunk.
        let pass_cursor = claim.state;
        let mut present: BTreeSet<ProviderKey> = BTreeSet::new();
        let mut totals = RunningApplied::default();
        let mut fetched = 0usize;
        let mut stream = provider.stream_email(
            account,
            pass_cursor.as_ref(),
            tuning.window,
            tuning.fetch_batch,
            tuning.chunk_size,
        );
        loop {
            let awaited = Instant::now();
            let next = stream.next().await;
            timing.add_fetching(awaited);
            let Some(item) = next else {
                // The stream ended cleanly: an additive pass has committed and
                // checkpointed its final chunk, so the cursor is already current.
                store.release_sync_scope(lease).await?;
                return Ok(totals.into_applied());
            };
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(other) => {
                    let _ = store.release_sync_scope(lease).await;
                    return Err(other.into());
                }
            };
            let count = chunk.changed.len();
            let total = chunk.total;
            let is_reconcile_final = chunk.is_reconcile_final();
            let projected = Instant::now();
            let (update, advance_to) = build_update(chunk, &mut present);
            let mut derived = derive_messages(changed_of(&update));
            for change in update.patched() {
                derived.push_state_change(project_state_change(change));
            }
            timing.add_deriving(projected);
            let stored = Instant::now();
            let observations =
                match recipients::observations(store, account, &scope, &update, sent).await {
                    Ok(observations) => observations,
                    Err(err) => {
                        let _ = store.release_sync_scope(lease).await;
                        return Err(err);
                    }
                };
            let batch = ApplyBatch::with_cursor(&update, &derived, &[], advance_to.as_ref())
                .with_recipient_observations(&observations);
            let applied_result = store.apply_sync_update(&lease, batch).await;
            timing.add_storing(stored);
            match applied_result {
                Ok(applied) => {
                    totals.add(applied);
                    fetched += count;
                    observer.committed(&SyncCommit {
                        scope: &scope,
                        fetched,
                        total,
                        upserted: changed_of(&update),
                        removed: removed_of(&update),
                        tombstoned: applied.tombstoned,
                    });
                    if is_reconcile_final {
                        // A reconcile pass ends on its final (tombstoning) chunk.
                        store.release_sync_scope(lease).await?;
                        return Ok(totals.into_applied());
                    }
                }
                Err(StoreError::StaleLease) if reclaims < MAX_STALE_RECLAIMS => {
                    // The lease was superseded mid-stream. The cursor is either
                    // checkpointed (additive) or never advanced (reconcile), so drop
                    // the partial stream and re-claim; the adapter re-syncs its
                    // transport on the next fetch.
                    reclaims += 1;
                    drop(stream);
                    continue 'restart;
                }
                Err(other) => {
                    let _ = store.release_sync_scope(lease).await;
                    return Err(other.into());
                }
            }
        }
    }
}

/// Accumulates per-chunk apply results into one pass total. Every field sums across
/// chunks: a delta's `removed` tombstones and pending-op reconciliations can land on
/// any chunk, and a reconcile pass tombstones only on its final chunk, so summing is
/// correct in both cases.
#[derive(Default)]
struct RunningApplied {
    upserted: usize,
    tombstoned: usize,
    reconciled: usize,
}

impl RunningApplied {
    fn add(&mut self, applied: SyncApplied) {
        self.upserted += applied.upserted;
        self.tombstoned += applied.tombstoned;
        self.reconciled += applied.reconciled;
    }

    fn into_applied(self) -> SyncApplied {
        SyncApplied {
            upserted: self.upserted,
            tombstoned: self.tombstoned,
            reconciled: self.reconciled,
        }
    }
}

/// Turns one chunk into the [`SyncUpdate`] to apply and the cursor to advance to.
///
/// - [`PassMode::Additive`]: a delta (upserts + explicit removals); advance to the chunk's
///   checkpoint.
/// - [`PassMode::Reconcile`]: intermediate chunks apply additively and hold the cursor while
///   `present` accumulates; the final chunk applies a snapshot against the whole accumulated
///   present set (tombstoning) and advances the cursor.
///
/// The chunk's state changes go **into the update** rather than beside it, so this driver and
/// the whole-scope [`drain_email`](engine_provider::stream) reach the same two conclusions from
/// the same code: a key that also arrived whole drops its patch (the object is the later word),
/// and a snapshot carries no partials at all, because it is already the scope's whole current
/// state. Carrying them alongside made the update an incomplete account of the pass — which is
/// how a message moved into Sent by a state change was invisible to the recipient observations.
fn build_update(
    chunk: EmailChunk,
    present: &mut BTreeSet<ProviderKey>,
) -> (SyncUpdate<Message>, Option<SyncState>) {
    let EmailChunk {
        mode,
        changed,
        patched,
        removed,
        present: page_present,
        advance_to,
        ..
    } = chunk;
    let update = match mode {
        PassMode::Additive => SyncUpdate::delta(changed, removed),
        PassMode::Reconcile => {
            present.extend(page_present);
            if advance_to.is_some() {
                SyncUpdate::snapshot(changed, core::mem::take(present))
            } else {
                SyncUpdate::delta(changed, removed)
            }
        }
    };
    (update.with_patched(patched), advance_to)
}

/// The upserted objects of an update (a delta's `changed` or a snapshot's `objects`).
fn changed_of(update: &SyncUpdate<Message>) -> &[Message] {
    match update {
        SyncUpdate::Delta { changed, .. } => changed,
        SyncUpdate::Snapshot { objects, .. } => objects,
    }
}

/// The explicitly-removed keys of an update (empty for a snapshot, whose removals
/// are computed by present-set diff inside the store).
fn removed_of(update: &SyncUpdate<Message>) -> &[ProviderKey] {
    match update {
        SyncUpdate::Delta { removed, .. } => removed,
        SyncUpdate::Snapshot { .. } => &[],
    }
}
