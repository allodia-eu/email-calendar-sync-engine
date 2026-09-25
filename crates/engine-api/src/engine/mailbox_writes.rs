//! Folder-tree changes on `Engine`: create, rename or move, trash and delete, through the
//! outbox, each followed by a re-read of the folder list.

use engine_core::ids::AccountId;
use engine_provider::Provider;
use engine_sync::{MailboxChange, MailboxEditOutcome, edit_mailbox, reconcile_folders};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

/// A folder change that went through, and whether the store has caught up with it.
#[derive(Debug)]
pub struct MailboxWrite {
    /// What the change resolved to.
    pub outcome: MailboxEditOutcome,
    /// Whether the folder list was re-read afterwards. A failed re-read is not a failed
    /// change: the server has it, and the next sync brings the store up to date.
    pub reconciled: Result<(), ApiError>,
}

impl Engine {
    /// Changes one account's folder tree through the durable outbox, then re-reads the
    /// folder list so the store holds the result.
    ///
    /// **Read
    /// [`Capabilities::mailbox_writes`](engine_provider::Capabilities::mailbox_writes)
    /// first.** A change that names a folder carries where the user saw it
    /// ([`MailboxPlace`](engine_sync::MailboxPlace)); before anything is sent the engine
    /// reads the folder list and refuses the change as a conflict if that folder has since
    /// been renamed, moved or removed elsewhere, rather than overwrite it. The same check
    /// runs when a change queued offline is replayed by
    /// [`drain_outbox`](Self::drain_outbox), after which the host re-reads the list with
    /// [`reconcile_folders`](Self::reconcile_folders).
    ///
    /// Trashing a folder moves it, its subfolders and their mail into `trash`, wherever
    /// the transport can file a folder there. Gmail cannot, so there its mail goes to Trash
    /// and its labels are removed. Deleting is permanent and meant for a folder already
    /// in Trash.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for a folder name no transport accepts, before
    /// anything is queued, and [`ApiError::Sync`] if the change is refused or fails: a
    /// retryable failure leaves it queued for the drainer; a conflict settles it.
    pub async fn edit_mailbox<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        change: &MailboxChange,
    ) -> Result<MailboxWrite, ApiError> {
        change
            .validate()
            .map_err(|err| ApiError::InvalidInput(err.to_string()))?;
        let outcome = edit_mailbox(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            change,
        )
        .await
        .map_err(map_sync_error)?;
        let reconciled = self.reconcile_folders(provider, account).await;
        Ok(MailboxWrite {
            outcome,
            reconciled,
        })
    }

    /// Re-reads one account's folder list into the store and forgets the mail of any
    /// folder it no longer holds.
    ///
    /// What a host runs after a drain pass that landed a folder change, since the drainer
    /// itself does not touch the store. A full sync does the same as its first step.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] when another sync holds the folder list, and
    /// [`ApiError::Sync`] if the list cannot be read or applied.
    pub async fn reconcile_folders<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<(), ApiError> {
        reconcile_folders(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map(|_| ())
            .map_err(map_sync_error)
    }
}
