//! The [`MailboxWrites`] half of the provider seam: changing an account's folder tree.
//!
//! A supertrait of [`Provider`](crate::Provider) for the reason
//! [`CalendarWrites`](crate::CalendarWrites) is one: `provider.rs` is at the file-length
//! limit, and the verb sits better beside the type it takes. A `P: Provider` exposes it
//! exactly as it would a method of its own.
//!
//! The verb defaults to rejecting, so an adapter that cannot change folders states that
//! with an empty impl and a capability set that says the same
//! ([`Capabilities::mailbox_writes`](crate::Capabilities::mailbox_writes)).

use async_trait::async_trait;
use engine_core::ids::AccountId;

// Named only by the doc links below, which rustdoc resolves against this module's scope.
#[allow(
    unused_imports,
    reason = "named by intra-doc links on the trait's methods"
)]
use crate::{Capabilities, ProviderError};
use crate::{MailboxEdit, MailboxEditReceipt, ProviderResult, error::unsupported};

/// The folder-write verb every adapter answers, rejecting by default.
#[async_trait]
pub trait MailboxWrites: Send + Sync {
    /// Applies one [`MailboxEdit`] to the account's folder tree.
    ///
    /// Outbox-mediated by the caller: a durable pending op precedes this side effect, and
    /// the caller has already checked the folder is where the user saw it. This method
    /// performs only the provider call.
    ///
    /// **A create that finds its folder already there succeeds** with that folder, and a
    /// delete that finds nothing succeeds too: both verbs sit behind a retryable op, so a
    /// retry routinely meets what the first attempt did.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A target the server no longer has, a
    /// destination that already holds a folder of that name, or a move into the folder's
    /// own subtree is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict). A name the
    /// transport cannot express (one carrying its hierarchy separator) is
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState), as
    /// is the default, which an adapter without [`Capabilities::mailbox_writes`] keeps.
    async fn edit_mailbox(
        &self,
        account: &AccountId,
        edit: &MailboxEdit,
    ) -> ProviderResult<MailboxEditReceipt> {
        let _ = (account, edit);
        Err(unsupported("folder changes"))
    }
}
