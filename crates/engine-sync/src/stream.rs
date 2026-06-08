//! Streaming mail sync: commit each email page as it lands, report progress.
//!
//! The responsive counterpart to [`crate::sync_mail`]. The whole-scope loop in
//! `lib.rs` claims, fetches, and applies one scope atomically; here the email scope
//! is driven page by page under a single lease so a host UI can render recent mail
//! and live "downloaded Y of X" feedback before the sync finishes. Only the final
//! page advances the cursor (`store-and-sync.md`), so a mid-stream crash re-runs the
//! pass from scratch idempotently.

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{SyncScope, SyncUpdate};
use engine_provider::{PageToken, Provider, SyncKind, SyncPage};
use engine_store::{ApplyBatch, LeaseRequest, Store, StoreError, SyncApplied, WorkerId};

use crate::{
    MAX_STALE_RECLAIMS, MailSyncReport, MailboxScope, SyncError, derive_messages, run_scope,
};

/// Progress for one streaming scope, reported after each page commits.
///
/// `fetched` is the number of objects committed so far this pass — already visible
/// to the host (a UI can render them) — and `total` is the provider's reported
/// denominator when it knows it (the `X` in "downloaded Y of X"). For a first
/// snapshot sync the very first page already carries `total`, so a host can show a
/// determinate bar immediately; an incremental delta typically reports `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncProgress {
    /// The scope being streamed.
    pub scope: SyncScope,
    /// Objects committed (and so host-visible) so far this pass.
    pub fetched: usize,
    /// The provider's total for the pass, if known.
    pub total: Option<usize>,
}

/// A sink the streaming sync notifies after each committed page, so a host can
/// surface live "downloading Y of X" feedback while a fresh sync fills in.
///
/// Implementations must be cheap and non-blocking (e.g. push onto a channel); the
/// sync awaits nothing on them. The blanket impl over `Fn(SyncProgress)` lets a
/// caller pass a closure directly.
pub trait ProgressSink: Send + Sync {
    /// Receives the running progress for a scope after a page commits.
    fn report(&self, progress: SyncProgress);
}

impl<F: Fn(SyncProgress) + Send + Sync> ProgressSink for F {
    fn report(&self, progress: SyncProgress) {
        self(progress);
    }
}

/// Syncs one account's mail like [`crate::sync_mail`], but **streams** the email
/// scope: each page of messages is committed as it arrives — so a host UI can show
/// recent mail and live progress before the whole sync finishes — and only the
/// final page advances the scope cursor. Mailboxes are still fetched whole (they
/// are small and must precede email). `progress` is notified after every committed
/// email page.
///
/// Email is paged newest-first by the provider, so the first visible rows are the
/// most recent. Intermediate pages commit additively without advancing the cursor;
/// a crash mid-stream therefore leaves the prior cursor intact and the next sync
/// re-runs the pass from scratch (idempotently) rather than skipping un-applied
/// pages (`store-and-sync.md`). `page_limit` bounds each page (`0` means the
/// provider's maximum).
///
/// # Errors
///
/// Returns [`SyncError`] if the provider fetch fails or the store rejects an apply
/// for a reason other than a recoverable `StaleLease`.
pub async fn sync_mail_streamed<P, S, K>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    page_limit: usize,
    progress: &K,
) -> Result<MailSyncReport, SyncError>
where
    P: Provider,
    S: Store,
    K: ProgressSink,
{
    let req = LeaseRequest::new(worker, ttl);
    let mailboxes = run_scope(store, account, &MailboxScope(provider), &req).await?;
    let email = stream_email(provider, store, account, &req, page_limit, progress).await?;
    Ok(MailSyncReport { mailboxes, email })
}

/// Streams the email scope page by page under one lease: each page commits
/// additively (cursor held) and only the last advances the cursor and — for a
/// snapshot pass — tombstones against the **accumulated** present set. A
/// `StaleLease` abandons the partial stream and restarts from a fresh claim; the
/// held cursor makes that safe.
async fn stream_email<P, S, K>(
    provider: &P,
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    page_limit: usize,
    progress: &K,
) -> Result<SyncApplied, SyncError>
where
    P: Provider,
    S: Store,
    K: ProgressSink,
{
    let scope = provider.email_scope(account);
    let mut reclaims = 0u32;
    'restart: loop {
        let claim = store
            .claim_sync_scope(account.clone(), &scope, req.clone())
            .await?;
        let lease = claim.lease;
        // The cursor every page of this pass resumes the *provider* fetch from; it
        // only advances in the store once the final page commits.
        let pass_cursor = claim.state;
        let mut present: BTreeSet<ProviderKey> = BTreeSet::new();
        let mut page_token: Option<PageToken> = None;
        let mut upserted = 0usize;
        let mut fetched = 0usize;
        loop {
            let page = provider
                .sync_email_page(
                    account,
                    pass_cursor.as_ref(),
                    page_token.as_ref(),
                    page_limit,
                )
                .await?;
            let SyncPage {
                kind,
                changed,
                removed,
                present: page_present,
                next_page,
                next_cursor,
                total,
            } = page;
            let is_last = next_page.is_none();
            let page_count = changed.len();
            let derived = derive_messages(&changed);
            // Intermediate pages apply additively (no tombstoning, cursor held); the
            // final page applies the real shape and advances the cursor.
            let update = match kind {
                SyncKind::Snapshot => {
                    present.extend(page_present);
                    if is_last {
                        // The final page tombstones against the whole accumulated
                        // set; `present` is not read after this, so move it.
                        SyncUpdate::snapshot(changed, core::mem::take(&mut present))
                    } else {
                        SyncUpdate::delta(changed, Vec::new())
                    }
                }
                SyncKind::Delta => SyncUpdate::delta(changed, removed),
            };
            let next_state = is_last.then_some(&next_cursor);
            let batch = ApplyBatch::with_cursor(&update, &derived, &[], next_state);
            match store.apply_sync_update(&lease, batch).await {
                Ok(applied) => {
                    upserted += applied.upserted;
                    fetched += page_count;
                    progress.report(SyncProgress {
                        scope: scope.clone(),
                        fetched,
                        total,
                    });
                    if is_last {
                        store.release_sync_scope(lease).await?;
                        // `applied` already carries this page's tombstone/reconcile
                        // counts; only `upserted` accumulates across pages.
                        return Ok(SyncApplied {
                            upserted,
                            ..applied
                        });
                    }
                    page_token = next_page;
                }
                Err(StoreError::StaleLease) if reclaims < MAX_STALE_RECLAIMS => {
                    // The lease was superseded mid-stream. The cursor was never
                    // advanced, so abandon the partial pass and re-claim afresh.
                    reclaims += 1;
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
