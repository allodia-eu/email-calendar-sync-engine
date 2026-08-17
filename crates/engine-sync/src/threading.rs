//! Rebuilding the thread index from the stored payloads — the repair path.
//!
//! **Threading is maintained as mail lands**, inside the apply transaction, from the stored
//! message-id graph (`store-and-sync.md`). This pass is not that: it is the whole-account
//! re-derivation a repair or a schema migration runs, and nothing calls it after an ordinary sync.
//! It rewrites the graph rows as well as the thread ids, because a repair that fixed only what the
//! graph decides would leave the next incremental assignment reading the same broken graph.
//!
//! Providers that assign their own thread ids (JMAP `Thread.id`, Gmail `threadId`,
//! Graph `conversationId`) set [`Message::thread`] during sync. Providers that do not —
//! notably IMAP — leave it `None`, so the engine **derives** it from the RFC 5322
//! `Message-ID` / `In-Reply-To` / `References` headers (`modeling.md`: those are
//! threading hints, not identity).
//!
//! Derivation is **account-wide and cross-folder**: a reply filed in Sent and its
//! original in the Inbox are distinct provider objects in distinct scopes, but they
//! share message-ids, so they belong to one conversation (the Outlook/Gmail
//! behavior). It therefore runs as a post-sync pass over all the account's stored
//! messages, not inside a single scope's [`derive`](crate::ScopeSyncer) step.
//!
//! The grouping is a union-find over the message-id graph: two messages are united if
//! they share any id they own or reference (so a duplicate of one message in two
//! folders, and a reply that references its parent, both unite). Each component gets a
//! [`ThreadId`] that is a pure function of the component (the lexicographically
//! smallest owned `Message-ID`, falling back to the smallest provider key), so it does
//! not depend on the order mail arrived in and a full resync reproduces it exactly.
//! Subject-based linking is deliberately omitted for now — it over-merges unrelated
//! mail; the header graph is the safe baseline.
//!
//! The pass runs over the messages that carry **no** thread id and the ones whose id it
//! [derived](engine_core::mail::ThreadProvenance::LocallyDerived) itself — a reply
//! synced long after its thread was first derived must still join it, which means
//! re-grouping mail that was already grouped. Messages the *provider* threaded are
//! excluded from the graph entirely and never rewritten (a stray `References` header
//! must not merge two threads the provider kept apart), so the pass is a no-op against
//! a JMAP account.
//!
//! Because it walks whole components rather than the one an arrival touches, it also repairs the
//! two shapes the incremental rule cannot see: a component a *deletion* split in two, and one
//! whose only owner was deleted, which keeps a name no remaining member owns.
//!
//! A merge can therefore **re-key** an existing thread: when a message owning a smaller
//! `Message-ID` joins, the component's id changes and every member is re-applied. Hosts
//! that key list rows on `thread_id` must tolerate that (`threading.md`).

use core::{ops::Range, time::Duration};
use std::collections::{BTreeSet, HashMap};

use engine_core::{
    ids::{AccountId, ProviderKey, ThreadId},
    mail::{Message, StoredContent},
    search_index::{MailRefRow, MailThreadRow, project_refs},
    sync::{ObjectKind, SyncScope, SyncUpdate},
};
use engine_store::{
    ApplyBatch, DerivedWrite, LeaseRequest, MailSelector, Store, StoreRead, WorkerId,
};

use crate::SyncError;

mod grouping;

use grouping::derive_thread_assignments;

/// What one [`rebuild_thread_index`] pass changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadRebuildReport {
    /// Messages written with a derived thread id they did not already carry — the
    /// newly grouped ones, plus any whose thread a merge re-keyed.
    pub messages_assigned: usize,
    /// Distinct derived threads spanning the account's derivable mail.
    pub threads: usize,
}

/// Rebuilds one account's derived thread ids and its message-id graph from the stored payloads,
/// grouping messages across all the account's mail scopes (folders) by their shared
/// `Message-ID`/`In-Reply-To`/`References` headers.
///
/// Reads every mail scope's payloads, computes the account-wide grouping, then re-applies the
/// changed messages per scope with their re-projected graph rows — **without advancing the scope
/// cursor** (it is a derivation, not a sync), so the next real sync still resumes from where it
/// left off. Lease-gated like sync.
///
/// **Not part of an ordinary sync.** An arrival is threaded inside its own apply, so running this
/// afterwards would re-read every payload in the account to confirm an answer already written.
/// Reach for it when the index is suspect — after a migration that introduced it, or a repair —
/// and it writes nothing over mail that is already right.
///
/// # Errors
///
/// Returns [`SyncError`] if a scope read, claim, or apply fails (a live competing
/// lease surfaces as the retryable [`StoreError::ScopeHeld`](engine_store::StoreError)).
pub async fn rebuild_thread_index<S>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ThreadRebuildReport, SyncError>
where
    S: Store + StoreRead,
{
    // Gather every mail scope's live messages (cross-folder is the point).
    let scopes: Vec<SyncScope> = store
        .account_scopes(account.clone())
        .await?
        .into_iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Message))
        .collect();
    // The payload alone, deliberately: threading reads the reference graph and the provider's
    // own thread id, both of which are content. The engine's *derived* ids come from the rows
    // below, which is the only place they live.
    //
    // One flat buffer with a range per scope, rather than a per-scope `Vec` *and* a copy of every
    // message in an account-wide one. This is the pass a migration or a repair runs, on a device
    // that has just updated, so holding a large mailbox's payloads twice is the difference that
    // matters here — the grouping needs them all at once, and the persist below needs them
    // grouped, and a range gives both from one copy.
    let mut all: Vec<StoredContent> = Vec::new();
    let mut per_scope: Vec<(SyncScope, Range<usize>)> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let start = all.len();
        for (_key, payload) in store.scope_objects(&scope).await? {
            if let Ok(content) = serde_json::from_value::<StoredContent>(payload) {
                all.push(content);
            }
        }
        per_scope.push((scope, start..all.len()));
    }

    let assignments = derive_thread_assignments(&all);
    let threads = assignments
        .values()
        .cloned()
        .collect::<BTreeSet<ThreadId>>()
        .len();

    // What each message's thread id *is* comes from the stored row, not from the payload the
    // graph was rebuilt out of. The row is where a thread id lives; a payload carries the one
    // it held when it was last written, so comparing against it would re-assign every message
    // on every pass.
    let stored = stored_thread_ids(store, account).await?;

    // Persist per scope: write the thread id of the messages whose id changed, and nothing
    // else about them, leaving the cursor untouched.
    let mut messages_assigned = 0usize;
    for (scope, range) in per_scope {
        let messages = &all[range];
        // The graph is rewritten for **every** message, not only the re-keyed ones. A repair is
        // reached for because the index is suspect, and the rows the next incremental assignment
        // reads are half of that index — repairing only what they decided would leave it reading
        // the same broken graph tomorrow.
        let msgid_refs: Vec<MailRefRow> = messages
            .iter()
            .flat_map(|message| {
                project_refs(message.id.key(), &message.envelope, message.thread.as_ref())
            })
            .collect();
        let updated: Vec<MailThreadRow> = messages
            .iter()
            .filter_map(|message| {
                let thread_id = assignments.get(message.id.key())?;
                // A provider-threaded message is not in `assignments` at all, so only the
                // engine's own ids are compared here.
                if stored.get(message.id.key()) == Some(thread_id) {
                    return None;
                }
                Some(MailThreadRow {
                    key: message.id.key().clone(),
                    thread_id: thread_id.clone(),
                })
            })
            .collect();
        if updated.is_empty() && msgid_refs.is_empty() {
            continue;
        }
        messages_assigned += updated.len();

        let claim = store
            .claim_sync_scope(
                account.clone(),
                &scope,
                LeaseRequest::new(worker.clone(), ttl),
            )
            .await?;
        // No objects: a derivation decides a thread id, and rewriting the payloads it read to
        // deliver one would carry every other column along with it — including the flags a
        // keyword change had just moved.
        let update: SyncUpdate<Message> = SyncUpdate::delta(Vec::new(), Vec::new());
        let derived = DerivedWrite {
            thread_assignments: updated,
            msgid_refs,
            ..DerivedWrite::empty()
        };
        let batch = ApplyBatch::with_cursor(&update, &derived, &[], None);
        match store.apply_sync_update(&claim.lease, batch).await {
            Ok(_) => store.release_sync_scope(claim.lease).await?,
            Err(err) => {
                let _ = store.release_sync_scope(claim.lease).await;
                return Err(err.into());
            }
        }
    }

    Ok(ThreadRebuildReport {
        messages_assigned,
        threads,
    })
}

/// Each of the account's messages mapped to the thread id its **stored row** carries.
///
/// An indexed read of the row table, which is where a thread id lives — cheap beside the
/// payload scan above, and the only source that reflects what the last pass actually wrote.
async fn stored_thread_ids<S: StoreRead>(
    store: &S,
    account: &AccountId,
) -> Result<HashMap<ProviderKey, ThreadId>, SyncError> {
    Ok(store
        .list_mail(
            core::slice::from_ref(account),
            MailSelector::Newest,
            usize::MAX,
        )
        .await?
        .into_iter()
        .filter_map(|row| Some((row.mail.key, row.mail.thread_id?)))
        .collect())
}
