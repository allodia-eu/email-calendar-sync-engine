//! Forgetting the mail of a folder the account no longer has, and the folder-list re-read a
//! folder change ends with.
//!
//! Where message sync is per folder (IMAP, Graph), a folder's messages live in a scope of
//! their own, and nothing removes that scope when the folder leaves the folder list. On IMAP
//! a folder's key is its path, so renaming or moving a folder, here or in another client,
//! leaves a whole copy of its mail under the old path: counted by nothing, synced by nothing,
//! and found by every search. The folder list is the authority on which folders exist, so
//! once it has been applied, a per-folder scope it does not name is dropped whole.

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::Mailbox,
    sync::ObjectKind,
};
use engine_provider::Provider;
use engine_store::{LeaseRequest, Store, StoreError, StoreRead, SyncApplied, WorkerId};

use crate::{MailboxScope, SyncError, run_scope};

/// Re-reads the account's folder list into the store, then forgets the mail of any folder it
/// no longer holds: the read-your-writes step after a folder change.
///
/// A folder change does not touch the store (`store-and-sync.md`: a write's response is a
/// receipt, not a document), so this is what makes a new, renamed or trashed folder appear.
/// It sends nothing but the list read, so a host may run it after a change landed from a
/// drain pass as well as after one made inline.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the list cannot be read, or [`SyncError::Store`] if it
/// cannot be applied or a forget fails. A folder list another worker is syncing is
/// [`StoreError::ScopeHeld`], which that worker's pass resolves.
pub async fn reconcile_folders<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<SyncApplied, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let req = LeaseRequest::new(worker, ttl);
    let applied = run_scope(store, account, &MailboxScope(provider), &req)
        .await?
        .into_applied()
        .0;
    forget_vanished_folders(store, account, &req).await?;
    Ok(applied)
}

/// Forgets every per-folder message scope of `account` whose folder the stored folder list
/// does not hold, returning how many it forgot.
///
/// Run it after the folder list has been applied from the server, never before: the stored
/// list is what it judges by. A list with no folders in it judges nothing, because an
/// account with no folders at all is far more likely to be a list that failed to arrive. A
/// scope another worker holds is left for the next pass.
///
/// # Errors
///
/// Returns [`SyncError::Store`] if the store cannot be read or a forget fails.
pub async fn forget_vanished_folders<S: Store + StoreRead>(
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
) -> Result<usize, SyncError> {
    let scopes = store.account_scopes(account.clone()).await?;
    let mut listed: BTreeSet<MailboxId> = BTreeSet::new();
    for scope in scopes
        .iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Mailbox))
    {
        for (_key, payload) in store.scope_objects(scope).await? {
            let mailbox: Mailbox = serde_json::from_value(payload)
                .map_err(|err| SyncError::Decode(format!("stored folder: {err}")))?;
            listed.insert(mailbox.id);
        }
    }
    if listed.is_empty() {
        return Ok(0);
    }

    let mut forgotten = 0;
    for scope in &scopes {
        let Some(folder) = scope.folder() else {
            continue;
        };
        if listed.contains(folder) || already_empty(store, account, scope).await? {
            continue;
        }
        match store
            .claim_sync_scope(account.clone(), scope, req.clone())
            .await
        {
            Ok(claim) => {
                store.forget_scope(claim.lease).await?;
                forgotten += 1;
            }
            Err(StoreError::ScopeHeld) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(forgotten)
}

/// Whether a scope holds neither mail nor a cursor: one already forgotten, which a claim
/// would only bump for nothing.
async fn already_empty<S: Store + StoreRead>(
    store: &S,
    account: &AccountId,
    scope: &engine_core::sync::SyncScope,
) -> Result<bool, SyncError> {
    Ok(store
        .load_sync_state(account.clone(), scope)
        .await?
        .is_none()
        && store.object_keys(scope).await?.is_empty())
}
