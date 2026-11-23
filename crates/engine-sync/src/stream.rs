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
//!
//! A pass asked for more than its scope's cursor covers widens: after the delta, it streams
//! [`Provider::widen_email`] under the same lease, and records the wider depth as covered only
//! in a commit of its own once the band has ended (`crate::window_cursor`).

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::{MailboxWindows, SyncScope, SyncState, Widening},
};
use engine_provider::{Provider, ProviderError};
use engine_store::{LeaseRequest, Store, StoreError, StoreRead, SyncApplied};

use crate::{
    MAX_STALE_RECLAIMS, SyncError, SyncObserver,
    keep_alive::KeepAlive,
    mail_report::SyncTiming,
    stream_drain::{Drained, Run, RunningApplied},
    window_cursor::{self, Phase, Plan},
};

/// How a streaming sync runs: the depth window, plus how it separates network
/// batching from commit granularity.
///
/// - [`window`](Self::window): the sync-depth bound (a snapshot/backfill fetches only mail on or
///   after it); the full history by default. It is the account's window, with any mailbox the host
///   deepened ([`MailboxWindows::deepen`]) held further back.
/// - [`fetch_batch`](Self::fetch_batch): objects per provider round trip (`0` = the provider's
///   protocol maximum). Larger = fewer round trips.
/// - [`chunk_size`](Self::chunk_size): objects committed and reported per chunk (`0` = one chunk
///   per batch). Smaller = rows appear sooner.
///
/// A large `fetch_batch` with a small `chunk_size` is the sweet spot: few round
/// trips *and* immediate row-by-row rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTuning {
    /// The sync-depth window per mailbox (full history by default).
    pub window: MailboxWindows,
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
            window: MailboxWindows::default(),
            fetch_batch,
            chunk_size,
        }
    }

    /// The responsive default: a large batch (few round trips) with a small chunk
    /// (rows surface almost immediately) — what an interactive mail client wants.
    #[must_use]
    pub fn responsive() -> Self {
        Self {
            window: MailboxWindows::default(),
            fetch_batch: 200,
            chunk_size: 3,
        }
    }

    /// A throughput-biased tuning: a large batch committed in larger chunks — for a
    /// background backfill where per-row latency does not matter.
    #[must_use]
    pub fn bulk() -> Self {
        Self {
            window: MailboxWindows::default(),
            fetch_batch: 500,
            chunk_size: 100,
        }
    }

    /// Bounds this tuning to a sync-depth `window` (builder-style): a plain
    /// [`SyncWindow`](engine_core::sync::SyncWindow) for the whole account, or
    /// [`MailboxWindows`] to hold some mailboxes further back.
    #[must_use]
    pub fn within(mut self, window: impl Into<MailboxWindows>) -> Self {
        self.window = window.into();
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
    pub(crate) tuning: &'a StreamTuning,
    /// Told about every committed chunk.
    pub(crate) observer: &'a O,
    /// The account's Sent mailboxes, resolved once for the whole pass.
    pub(crate) sent: &'a BTreeSet<MailboxId>,
}

/// What one folder's pass did, and whether its stored mail now reaches the depth asked.
pub(crate) struct FolderOutcome {
    /// What the pass applied.
    pub(crate) applied: SyncApplied,
    /// Whether the scope's recorded depth is now the one asked: `false` when a widening is
    /// still to finish, on a pass fenced off by another.
    pub(crate) reached: bool,
}

/// Streams one folder's mail into the store, filling `timing` as it goes.
///
/// One claim runs the plan its stored cursor calls for ([`Plan`]): a fresh pass, a delta, or a
/// delta and then the widening to the depth asked. The delta goes first, so new mail shows before
/// old mail and the band starts from a current cursor; both run under the one lease.
///
/// Everything that is the same for every folder of a pass travels in [`FolderPass`], so what
/// varies per call is the folder's provider, the account, and where its measurements go.
pub(crate) async fn stream_email<P, S, O>(
    provider: &P,
    account: &AccountId,
    pass: &FolderPass<'_, S, O>,
    timing: &mut SyncTiming,
) -> Result<FolderOutcome, SyncError>
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
    // The depth this scope fetches under, and records with its cursor: a folder-bound scope's
    // is its own folder's window, an account-wide one's every mailbox's (`window_cursor`).
    let depth = depth_of(&scope, &tuning.window);
    let mut reclaims = 0u32;
    // Set when a commit was refused as stale: the cursor this pass had left, and what it applied.
    let mut fenced: Option<(Option<SyncState>, RunningApplied)> = None;
    loop {
        let claimed = store
            .claim_sync_scope(account.clone(), &scope, req.clone())
            .await;
        // A cursor that moved while this pass was fenced off was cleared, or advanced by the
        // pass that took the scope. A host clears one to change what a pass fetches, so starting
        // over under this pass's own window would undo the clear; the next pass takes it up.
        // A scope already held is that next pass, and this one ends rather than report busy.
        if let Some((left, applied)) = fenced.take() {
            let ended = FolderOutcome {
                applied: applied.into_applied(),
                reached: false,
            };
            match claimed {
                Err(StoreError::ScopeHeld) => return Ok(ended),
                Ok(claim) if claim.state != left => {
                    store.release_sync_scope(claim.lease).await?;
                    return Ok(ended);
                }
                _ => {}
            }
        }
        let claim = claimed?;
        let lease = claim.lease;
        let (first, widening) = Plan::new(claim.state.clone(), &depth).into_parts();
        let mut run = Run {
            store,
            req,
            account,
            scope: &scope,
            tuning,
            observer,
            sent,
            lease: &lease,
            keep_alive: KeepAlive::new(store, &lease, req.ttl),
            left: claim.state,
            totals: RunningApplied::default(),
        };
        let phases = run_plan(provider, &mut run, first, widening.as_ref(), &depth, timing).await;
        match phases {
            Ok(Drained::Ended) => {
                let applied = run.totals.into_applied();
                store.release_sync_scope(lease).await?;
                return Ok(FolderOutcome {
                    applied,
                    reached: true,
                });
            }
            // The lease was superseded mid-stream. The cursor is either checkpointed
            // (additive) or never advanced (reconcile), so re-claim; the adapter re-syncs
            // its transport on the next fetch.
            Ok(Drained::Stale) if reclaims < MAX_STALE_RECLAIMS => {
                reclaims += 1;
                fenced = Some((run.left, run.totals));
            }
            Ok(Drained::Stale) => {
                let _ = store.release_sync_scope(lease).await;
                return Err(StoreError::StaleLease.into());
            }
            Err(err) => {
                let _ = store.release_sync_scope(lease).await;
                return Err(err);
            }
        }
    }
}

/// Runs the first stream of a claimed pass and, when the plan widens, the band after it, ending
/// with the commit that records the depth asked as covered.
async fn run_plan<P, S, O>(
    provider: &P,
    run: &mut Run<'_, S, O>,
    first: Option<SyncState>,
    widening: Option<&Widening>,
    depth: &MailboxWindows,
    timing: &mut SyncTiming,
) -> Result<Drained, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
    O: SyncObserver,
{
    let (fetch_batch, chunk_size) = (run.tuning.fetch_batch, run.tuning.chunk_size);
    let first_stamp = |cursor| window_cursor::stamp(cursor, depth, widening, Phase::First);
    let stream = provider.stream_email(
        run.account,
        first.as_ref(),
        depth.clone(),
        fetch_batch,
        chunk_size,
    );
    if let Drained::Stale = run.drain(stream, &first_stamp, false, timing).await? {
        return Ok(Drained::Stale);
    }
    let Some(widening) = widening else {
        return Ok(Drained::Ended);
    };
    // The delta ends on a cursor, which the band keeps. A widening is planned only from a stored
    // cursor, and a delta only ever moves one on.
    let left = run
        .left
        .clone()
        .ok_or_else(|| ProviderError::invalid_state("a widening began with no stored cursor"))?;
    let cursor = window_cursor::provider_cursor(left);
    let band_stamp = |cursor| window_cursor::stamp(cursor, depth, Some(widening), Phase::Band);
    let band = provider.widen_email(run.account, &cursor, widening, fetch_batch, chunk_size);
    if let Drained::Stale = run.drain(band, &band_stamp, true, timing).await? {
        return Ok(Drained::Stale);
    }
    // Only now does the covered depth become the one asked: a pass killed before this commit
    // continues the widening rather than claim mail it did not fetch.
    let ended = run
        .left
        .clone()
        .map_or(cursor, window_cursor::provider_cursor);
    run.record(window_cursor::stamp(ended, depth, None, Phase::First))
        .await
}
/// The depth `scope` fetches under: its own folder's window where it is bound to one, else
/// the account's with every mailbox held further back.
fn depth_of(scope: &SyncScope, windows: &MailboxWindows) -> MailboxWindows {
    folder_of(scope).map_or_else(
        || windows.clone(),
        |mailbox| MailboxWindows::new(windows.window_for(mailbox)),
    )
}

/// The one mailbox a mail scope is bound to, where the protocol syncs mail per folder (IMAP,
/// Graph). `None` for a scope that holds the whole account's mail.
pub(crate) fn folder_of(scope: &SyncScope) -> Option<&MailboxId> {
    match scope {
        SyncScope::ImapMailbox { mailbox, .. }
        | SyncScope::GraphFolder {
            folder: mailbox, ..
        } => Some(mailbox),
        _ => None,
    }
}
