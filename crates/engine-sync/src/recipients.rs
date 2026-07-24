//! Sent-role resolution, one-time backfill, and coverage recording.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Mailbox, MailboxRole, Message},
    recipient::{RecipientCoverage, RecipientObservation, observe_sent_recipients},
    sync::{ObjectKind, SyncUpdate, SyncWindow},
};
use engine_store::{ContactStore, StoreRead};

use crate::{SyncError, changed_objects};

/// Version of the derivation from stored sent messages to recipient rows.
const INTERACTION_INDEX_VERSION: u32 = 1;

/// Reads every normalized Sent-role mailbox currently stored for an account.
pub(crate) async fn sent_mailboxes<S>(
    store: &S,
    account: &AccountId,
) -> Result<BTreeSet<MailboxId>, SyncError>
where
    S: StoreRead,
{
    let mut sent = BTreeSet::new();
    for scope in store.account_scopes(account.clone()).await? {
        if scope.object_kind() != Some(ObjectKind::Mailbox) {
            continue;
        }
        for (_, payload) in store.scope_objects(&scope).await? {
            let mailbox: Mailbox = serde_json::from_value(payload)
                .map_err(|error| SyncError::Decode(error.to_string()))?;
            if mailbox.role == Some(MailboxRole::Sent) {
                sent.insert(mailbox.id);
            }
        }
    }
    Ok(sent)
}

/// Derives idempotent observations for changed messages.
pub(crate) fn observations(
    account: &AccountId,
    update: &SyncUpdate<Message>,
    sent: &BTreeSet<MailboxId>,
) -> Vec<RecipientObservation> {
    changed_objects(update)
        .iter()
        .flat_map(|message| observe_sent_recipients(account, message, sent))
        .collect()
}

/// Backfills previously stored message rows once without forcing mail resync.
pub(crate) async fn backfill<S>(
    store: &S,
    account: &AccountId,
    sent: &BTreeSet<MailboxId>,
) -> Result<bool, SyncError>
where
    S: StoreRead + ContactStore,
{
    // Check the version *before* the scan. This runs on every mail sync, but the work
    // is one-time: without this guard a 50k-message account re-walked every scope and
    // deserialized every stored `Message` on each sync, only for
    // `apply_recipient_backfill` to reject the result as already-applied.
    if store
        .recipient_index_version(account)
        .await?
        .is_some_and(|current| current >= INTERACTION_INDEX_VERSION)
    {
        return Ok(false);
    }
    let mut observations = Vec::new();
    for scope in store.account_scopes(account.clone()).await? {
        if scope.object_kind() != Some(ObjectKind::Message) {
            continue;
        }
        for (_, payload) in store.scope_objects(&scope).await? {
            let message: Message = serde_json::from_value(payload)
                .map_err(|error| SyncError::Decode(error.to_string()))?;
            observations.extend(observe_sent_recipients(account, &message, sent));
        }
    }
    store
        .apply_recipient_backfill(account.clone(), INTERACTION_INDEX_VERSION, &observations)
        .await
        .map_err(Into::into)
}

/// Persists the explicit observation-coverage statement.
pub(crate) async fn record_coverage<S>(
    store: &S,
    account: &AccountId,
    window: SyncWindow,
    sent_identified: bool,
) -> Result<(), SyncError>
where
    S: ContactStore,
{
    store
        .set_recipient_coverage(&RecipientCoverage {
            account: account.clone(),
            window,
            sent_collection_identified: sent_identified,
        })
        .await?;
    Ok(())
}
