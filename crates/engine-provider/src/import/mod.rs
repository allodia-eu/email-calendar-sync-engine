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
//! [`MailImporter`] is the mail half, [`CalendarImporter`] the calendar half and
//! [`ContactImporter`] the contact half (the `calendar` and `contact` modules say where they
//! differ from mail). [`RulesImporter`] carries an account's mail rules and its out-of-office
//! reply, which are settings rather than objects: it appends rules after the target's own and
//! never replaces a reply the target holds (the `rules` module). An adapter that can import
//! implements it; one that cannot does not, so there is no capability flag to check. A host that
//! picks its adapter at runtime holds it as a `Box<dyn MailImporter>` or an `Arc<dyn MailImporter>`
//! beside its `dyn Provider`, and both are importers themselves, so a function generic over
//! `I: MailImporter` takes either.

mod calendar;
#[cfg(feature = "import-conformance")]
pub mod conformance;
mod contact;
mod message;
mod reply;
mod rules;

use std::sync::Arc;

use async_trait::async_trait;
pub use calendar::{CalendarImportLimits, CalendarImporter, EventImport, ImportedEvent};
pub use contact::{ContactImport, ContactImportLimits, ContactImporter, ImportedContact};
use engine_core::{
    ids::{AccountId, MailboxId},
    sync::SyncState,
};
pub use message::{ImportedMessage, MailImportLimits, MessageImport};
pub use reply::import_reply;
pub use rules::{ImportDialect, RulesImporter, append_rules};

use crate::{ProviderError, ProviderResult};

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

/// Whether `failed`, one item's failure, reaches every item after it, so an importer ends its
/// batch there and answers the rest [`not_sent`] without a request: a throttle, a credential
/// the target refused, or no credential at all
/// ([`is_credential_failure`](ProviderError::is_credential_failure), whatever its class: a
/// source that is down or hung would cost every later item another ask and its deadline). A
/// failure about one item, or about one exchange, ends nothing.
///
/// # Examples
///
/// ```
/// use engine_provider::{ProviderError, import::ends_batch};
///
/// assert!(ends_batch(&ProviderError::rate_limited("429", None)));
/// assert!(!ends_batch(&ProviderError::item_unavailable("too large")));
/// ```
#[must_use]
pub fn ends_batch(failed: &ProviderError) -> bool {
    use engine_core::error::FailureClass;
    failed.is_credential_failure()
        || matches!(
            failed.class(),
            FailureClass::RateLimited | FailureClass::Authentication
        )
}

/// The answer for a message a batch never sent, because `ending` ended the batch first: an
/// importer answers every message after the failure that ended its batch with this, so the
/// count of answers still matches.
///
/// Two endings reach every message of the account, and carry over: a throttle, with the wait
/// it named, and a refused credential. A host then schedules the message with the rest of the
/// batch, after the wait or after a new credential. An ending where no credential could be had
/// ([`is_credential_failure`](ProviderError::is_credential_failure)) reaches every message too,
/// and carries over whole, with its class and wait: a source that is down is `Retryable`, and a
/// credential that may not go there or cannot be presented stays `Permanent` (or
/// `InvalidState`), so a host does not retry it for ever. Any other ending was about
/// the exchange or another message (a connection lost, an answer that could not be read, a message
/// that may be stored), so this one, never sent, is plainly retryable: it neither
/// [may be stored](ProviderError::requires_confirmation) nor is
/// [unavailable](ProviderError::item_unavailable). Its detail never repeats the ending's text.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use engine_provider::{ProviderError, import::not_sent};
///
/// let throttled = ProviderError::rate_limited("429", Some(Duration::from_secs(30).into()));
/// let unsent = not_sent(&throttled);
/// assert_eq!(unsent.class(), throttled.class());
/// assert_eq!(unsent.retry_after(), throttled.retry_after());
/// ```
#[must_use]
pub fn not_sent(ending: &ProviderError) -> ProviderError {
    use engine_core::error::FailureClass;
    if ending.is_credential_failure() {
        return ending.reworded("not sent: no credential could be had for the batch");
    }
    match ending.class() {
        FailureClass::RateLimited => ProviderError::rate_limited(
            "not sent: the target throttled the batch",
            ending.retry_after(),
        ),
        FailureClass::Authentication => {
            ProviderError::authentication("not sent: the target refused the credential")
        }
        _ => ProviderError::retryable("not sent: an earlier failure ended the batch"),
    }
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
