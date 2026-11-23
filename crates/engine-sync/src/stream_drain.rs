//! Committing one [`EmailStream`] into the store, chunk by chunk, under a lease already held.
//!
//! An email pass drains one stream, or two when it widens: the delta, then the band
//! ([`crate::window_cursor`]). Both are applied the same way: each chunk becomes an update by the
//! shared window and patch rules ([`EmailChunk::into_update`](engine_provider::EmailChunk)), its
//! cursor is stamped with the depth the pass records, and the commit is reported. What differs
//! between them is only how the cursor is stamped and whether the commits are a widening's.

use std::{collections::BTreeSet, time::Instant};

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::Message,
    search_index::project_state_change,
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::EmailStream;
use engine_store::{
    ApplyBatch, LeaseRequest, Store, StoreError, StoreRead, SyncApplied, SyncLease,
};
use futures_util::StreamExt;

use crate::{
    StreamTuning, SyncCommit, SyncError, SyncObserver, derive_messages, keep_alive::KeepAlive,
    mail_report::SyncTiming, recipients, sent_copies,
};

/// How a drained stream ended.
pub(crate) enum Drained {
    /// It ended cleanly, or on the final chunk of a reconcile, with every chunk committed.
    Ended,
    /// A commit was refused because another pass took the lease.
    Stale,
}

/// Everything one claimed pass commits through, for as long as it holds its lease.
pub(crate) struct Run<'r, S, O> {
    pub(crate) store: &'r S,
    pub(crate) req: &'r LeaseRequest,
    pub(crate) account: &'r AccountId,
    pub(crate) scope: &'r SyncScope,
    /// The depth and batching the pass asks for; every additive chunk is judged by its window.
    pub(crate) tuning: &'r StreamTuning,
    pub(crate) observer: &'r O,
    pub(crate) sent: &'r BTreeSet<MailboxId>,
    pub(crate) lease: &'r SyncLease,
    pub(crate) keep_alive: KeepAlive<'r, S>,
    /// The cursor the store holds for the scope, as this pass last left it.
    pub(crate) left: Option<SyncState>,
    pub(crate) totals: RunningApplied,
}

impl<S, O> Run<'_, S, O>
where
    S: Store + StoreRead,
    O: SyncObserver,
{
    /// Commits `stream` chunk by chunk, stamping each cursor with `stamp`. `widening` marks the
    /// commits as a widening's for the observer. On an error the lease is still held.
    pub(crate) async fn drain(
        &mut self,
        mut stream: EmailStream<'_>,
        stamp: &(dyn Fn(SyncState) -> SyncState + Sync),
        widening: bool,
        timing: &mut SyncTiming,
    ) -> Result<Drained, SyncError> {
        let mut present: BTreeSet<ProviderKey> = BTreeSet::new();
        // A message a chunk sends again (an IMAP backfill's settled attachment flag) is written,
        // but counted once, so progress never runs past its total.
        let mut counted: BTreeSet<ProviderKey> = BTreeSet::new();
        let mut fetched = 0usize;
        loop {
            let awaited = Instant::now();
            let next = self.keep_alive.around(stream.next()).await;
            timing.add_fetching(awaited);
            // A clean end: an additive pass has committed and checkpointed its final chunk.
            let Some(item) = next else {
                return Ok(Drained::Ended);
            };
            let chunk = item?;
            let count = chunk
                .changed
                .iter()
                .filter(|message| counted.insert(message.id.key().clone()))
                .count();
            let repeats = chunk.changed.len() - count;
            let total = chunk.total;
            let is_reconcile_final = chunk.is_reconcile_final();
            let projected = Instant::now();
            // The shared window and patch rules (`EmailChunk::into_update`). On the snapshot path
            // depth is the adapter's, and `prune_account_mail_outside_window` is the backstop.
            let (update, advance_to) = chunk.into_update(&self.tuning.window, &mut present);
            let advance_to = advance_to.map(stamp);
            let mut derived = derive_messages(changed_of(&update));
            for change in update.patched() {
                derived.push_state_change(project_state_change(change));
            }
            timing.add_deriving(projected);
            let stored = Instant::now();
            let applied = self.apply(&update, &derived, advance_to.as_ref()).await;
            timing.add_storing(stored);
            match applied {
                Ok(mut applied) => {
                    if advance_to.is_some() {
                        self.left.clone_from(&advance_to);
                    }
                    applied.upserted = applied.upserted.saturating_sub(repeats);
                    self.totals.add(applied);
                    fetched += count;
                    self.observer.committed(&SyncCommit {
                        scope: self.scope,
                        fetched,
                        total,
                        upserted: changed_of(&update),
                        removed: removed_of(&update),
                        tombstoned: applied.tombstoned,
                        widening,
                    });
                    // A reconcile ends on its final (tombstoning) chunk.
                    if is_reconcile_final {
                        return Ok(Drained::Ended);
                    }
                }
                Err(SyncError::Store(StoreError::StaleLease)) => return Ok(Drained::Stale),
                Err(other) => return Err(other),
            }
        }
    }

    /// Moves the stored cursor to `cursor` alone, in a commit of its own: how a widening records
    /// that its band ended.
    pub(crate) async fn record(&mut self, cursor: SyncState) -> Result<Drained, SyncError> {
        let update = SyncUpdate::delta(Vec::new(), Vec::new());
        let derived = derive_messages(&[]);
        match self.apply(&update, &derived, Some(&cursor)).await {
            Ok(_) => {
                self.left = Some(cursor);
                Ok(Drained::Ended)
            }
            Err(SyncError::Store(StoreError::StaleLease)) => Ok(Drained::Stale),
            Err(other) => Err(other),
        }
    }

    async fn apply(
        &self,
        update: &SyncUpdate<Message>,
        derived: &engine_store::DerivedWrite,
        advance_to: Option<&SyncState>,
    ) -> Result<SyncApplied, SyncError> {
        let (store, account, scope) = (self.store, self.account, self.scope);
        let observations =
            recipients::observations(store, account, scope, update, self.sent).await?;
        let confirmed =
            sent_copies::reconciliations(store, account, scope, update, self.sent).await?;
        let batch = ApplyBatch::with_cursor(update, derived, &confirmed, advance_to)
            .with_recipient_observations(&observations);
        let batch = crate::read_since(batch, self.lease, self.req);
        Ok(store.apply_sync_update(self.lease, batch).await?)
    }
}

/// Accumulates per-chunk apply results into one pass total. Every field sums across
/// chunks: a delta's `removed` tombstones and pending-op reconciliations can land on
/// any chunk, and a reconcile pass tombstones only on its final chunk, so summing is
/// correct in both cases.
#[derive(Default)]
pub(crate) struct RunningApplied {
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

    pub(crate) fn into_applied(self) -> SyncApplied {
        SyncApplied {
            upserted: self.upserted,
            tombstoned: self.tombstoned,
            reconciled: self.reconciled,
        }
    }
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
