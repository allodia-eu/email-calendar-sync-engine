//! Storing an object that already exists, as it was: the import seam (the `import` feature).
//!
//! An import is not a sync write. A sync write is a user's change, made through the outbox, and
//! it may re-assemble the object and schedule meetings; an import stores an object that already
//! exists, byte for byte, with the state it had, and tells nobody. Any host that does that uses
//! it: a migration moving a mailbox between providers, a webmail storing an uploaded `.eml` in a
//! folder, an app importing a file. It sits behind a feature and in traits of its own because
//! it is not a sync verb: the sync [`Provider`](crate::Provider) contract does not change, and a
//! host that never imports never compiles it.
//!
//! [`MailImporter`] is the mail half. An adapter that can import implements it; one that cannot
//! does not, so there is no capability flag to check. A host that picks its adapter at runtime
//! holds it as a `Box<dyn MailImporter>` or an `Arc<dyn MailImporter>` beside its
//! `dyn Provider`, and both are importers themselves, so a function generic over
//! `I: MailImporter` takes either.

mod message;

use std::sync::Arc;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId},
    sync::SyncState,
};
pub use message::{ImportedMessage, MailImportLimits, MessageImport};

// Named only by the doc links below, which rustdoc resolves against this module's scope.
#[allow(
    unused_imports,
    reason = "named by intra-doc links on the trait's methods"
)]
use crate::ProviderError;
use crate::ProviderResult;

/// Imports messages into a target account.
#[async_trait]
pub trait MailImporter: Send + Sync {
    /// The limits the target states for what it imports ([`MailImportLimits`]).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] when the target cannot be asked.
    async fn mail_import_limits(&self, account: &AccountId) -> ProviderResult<MailImportLimits>;

    /// The target's cursor for `mailbox`, read now: a delta from it, through the target's own
    /// [`Provider::stream_email`](crate::Provider::stream_email), lists every message stored in
    /// `mailbox` after this call. A target whose cursor covers the whole account (JMAP) answers
    /// that one for any mailbox.
    ///
    /// A host records it before a batch's first write, so after a crash it can find what that
    /// batch stored without listing the mailbox whole.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] when the target cannot be asked, or does not know
    /// `mailbox`.
    async fn import_cursor(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
    ) -> ProviderResult<SyncState>;

    /// Imports `batch`, answering each message in order: the answer at index `i` is the outcome
    /// of `batch[i]`, and there are as many answers as messages.
    ///
    /// What an answer means:
    ///
    /// - `Ok`: the message is on the target, stored now or [already
    ///   there](ImportedMessage::already_there), with the metadata the target
    ///   [kept](ImportedMessage::kept).
    /// - An error with [`requires_confirmation`](ProviderError::requires_confirmation): the message
    ///   **may** have been stored. Its request went out and no answer came back. Find it (from the
    ///   [`import_cursor`](Self::import_cursor)) before sending it again.
    /// - Any other error: the message was not stored. One that is
    ///   [`item_unavailable`](ProviderError::item_unavailable) will never be (too large, too many
    ///   mailboxes, refused by the server); one that is retryable can be sent again.
    ///
    /// A failure that ends the batch (a lost connection, an expired credential) answers every
    /// message it left unanswered, so the count still matches.
    async fn import_messages(
        &self,
        account: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>>;
}

/// A boxed importer is an importer, delegating every method to the box's contents, so a host
/// that holds its adapter behind dynamic dispatch still passes it to code generic over
/// [`MailImporter`].
#[async_trait]
impl<T: MailImporter + ?Sized> MailImporter for Box<T> {
    async fn mail_import_limits(&self, account: &AccountId) -> ProviderResult<MailImportLimits> {
        (**self).mail_import_limits(account).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
    ) -> ProviderResult<SyncState> {
        (**self).import_cursor(account, mailbox).await
    }

    async fn import_messages(
        &self,
        account: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        (**self).import_messages(account, batch).await
    }
}

/// A shared importer is an importer, as for [`Box`]: a host that keeps one adapter as both its
/// `Arc<dyn Provider>` and its `Arc<dyn MailImporter>` passes either on.
#[async_trait]
impl<T: MailImporter + ?Sized> MailImporter for Arc<T> {
    async fn mail_import_limits(&self, account: &AccountId) -> ProviderResult<MailImportLimits> {
        (**self).mail_import_limits(account).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
    ) -> ProviderResult<SyncState> {
        (**self).import_cursor(account, mailbox).await
    }

    async fn import_messages(
        &self,
        account: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        (**self).import_messages(account, batch).await
    }
}

#[cfg(test)]
mod tests;
