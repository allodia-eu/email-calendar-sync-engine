//! A send nobody could vouch for, resolved by the copy a sync finds filed in Sent.
//!
//! An op in `NeedsConfirmation` is a send whose outcome is unknown: an SMTP acknowledgement
//! lost after `DATA`, or a process that ended mid-send (`interrupted_outcome`). The copy the
//! server filed in Sent is the evidence that it went, and it carries the `Message-ID` the draft
//! was sent under, so a sync that brings it back resolves the op in the same transaction
//! (`store-and-sync.md`, "Reconciliation is re-validated in the transaction").
//!
//! Two limits keep that evidence honest. **Only Sent counts**: a draft saved to the server
//! carries the same `Message-ID`, and a copy in Drafts proves nothing was delivered. **Only
//! `NeedsConfirmation` is planned**: a send still in flight records its own outcome, and
//! resolving it from under its worker would race the receipt.

use std::collections::{BTreeSet, HashMap};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{Message, StoredContent},
    sync::{Keyed, SyncScope, SyncUpdate},
    write::{PendingOpId, PendingOpKind},
};
use engine_provider::Draft;
use engine_store::{PendingOpState, PendingReconciliation, StoreRead};

use crate::{SyncError, changed_objects};

/// The sends awaiting confirmation that `update` shows filed in Sent, planned to resolve to
/// the key of the copy that proves them.
///
/// Reads the outbox only when the update files something in Sent, and a stored payload only
/// for a message a state change moved there (JMAP and Gmail send by moving the draft, so the
/// copy arrives as a change to a key the store already holds).
///
/// # Errors
///
/// Returns [`SyncError`] if a store read fails or a stored payload cannot be decoded.
pub(crate) async fn reconciliations<S>(
    store: &S,
    account: &AccountId,
    scope: &SyncScope,
    update: &SyncUpdate<Message>,
    sent: &BTreeSet<MailboxId>,
) -> Result<Vec<PendingReconciliation>, SyncError>
where
    S: StoreRead + ?Sized,
{
    let mut filed: Vec<(ProviderKey, Vec<MessageIdHeader>)> = changed_objects(update)
        .iter()
        .filter(|message| in_sent(message.mailboxes.iter(), sent))
        .map(|message| {
            (
                message.provider_key().clone(),
                message.envelope.message_id.clone(),
            )
        })
        .collect();
    let moved: Vec<&ProviderKey> = update
        .patched()
        .iter()
        .filter(|change| {
            change
                .state
                .mailboxes
                .as_ref()
                .is_some_and(|mailboxes| in_sent(mailboxes.iter(), sent))
        })
        .map(|change| &change.key)
        .collect();
    if filed.is_empty() && moved.is_empty() {
        return Ok(Vec::new());
    }

    let mut awaiting = awaiting_confirmation(store, account).await?;
    if awaiting.is_empty() {
        return Ok(Vec::new());
    }
    for key in moved {
        let Some(payload) = store.object_payload(scope, key).await? else {
            continue;
        };
        let content: StoredContent =
            serde_json::from_value(payload).map_err(|err| SyncError::Decode(err.to_string()))?;
        filed.push((key.clone(), content.envelope.message_id));
    }

    let mut plans = Vec::new();
    for (key, message_ids) in filed {
        for message_id in message_ids {
            if let Some(op) = awaiting.remove(&message_id) {
                plans.push(PendingReconciliation::new(
                    op,
                    PendingOpState::NeedsConfirmation,
                    key.clone(),
                ));
            }
        }
    }
    Ok(plans)
}

/// Whether any of `mailboxes` is a Sent mailbox.
fn in_sent<'a>(
    mut mailboxes: impl Iterator<Item = &'a MailboxId>,
    sent: &BTreeSet<MailboxId>,
) -> bool {
    mailboxes.any(|mailbox| sent.contains(mailbox))
}

/// The account's sends awaiting confirmation, by the `Message-ID` each was sent under.
async fn awaiting_confirmation<S>(
    store: &S,
    account: &AccountId,
) -> Result<HashMap<MessageIdHeader, PendingOpId>, SyncError>
where
    S: StoreRead + ?Sized,
{
    let mut awaiting = HashMap::new();
    for row in store.list_pending_ops(account.clone()).await? {
        if row.kind != Some(PendingOpKind::MailSubmit)
            || row.state != PendingOpState::NeedsConfirmation
        {
            continue;
        }
        // A payload this build cannot read stays listed for the host to answer; it is not
        // evidence of anything here.
        let Ok(draft) = serde_json::from_value::<Draft>(row.payload) else {
            continue;
        };
        awaiting.insert(draft.message_id, row.id);
    }
    Ok(awaiting)
}
