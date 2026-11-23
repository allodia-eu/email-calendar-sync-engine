//! The out-of-office reply as a capability: [`Vacation`], reached through
//! [`Provider::vacation`](crate::Provider::vacation).
//!
//! A reply is an account setting, not a message, so every host writes it directly, guarded by the
//! version it read, and never through the outbox; the store keeps no copy
//! (`filters-and-out-of-office.md`, decision 1). A sync host reads it when its settings screen
//! opens, a direct host per request, and a transfer host once per migration.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    vacation::{VacationReceipt, VacationResponse, VacationVocabulary},
};

use crate::{ProviderError, ProviderResult, WriteGuard};

/// An account's out-of-office reply: read it, and write it.
///
/// An adapter that can implements this and answers `Some(self)` from
/// [`Provider::vacation`](crate::Provider::vacation), and only for an account that has the
/// setting at all, so the answer is the capability check.
#[async_trait]
pub trait Vacation: Send + Sync {
    /// What this account's reply can hold. A host offers only these parts, and
    /// [`set_vacation`](Self::set_vacation) refuses a reply that uses any other
    /// ([`refuse_unheld_vacation`]).
    fn vocabulary(&self) -> VacationVocabulary;

    /// Whether a write built on a stale read is refused ([`WriteGuard::Enforced`]) or silently
    /// wins ([`WriteGuard::Absent`]).
    fn guard(&self) -> WriteGuard;

    /// The account's reply as the server holds it now, with the version a write built on it
    /// names.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the server cannot be asked or answers no reply.
    async fn read_vacation(&self, account: &AccountId) -> ProviderResult<VacationResponse>;

    /// Replaces the account's reply with `new`, refused if it moved since `base` was read.
    ///
    /// `base` is the reply as the caller read it: its [`version`](VacationResponse::version)
    /// guards the write where the transport can ([`guard`](Self::guard)), and its raw form
    /// supplies what the model does not carry. A `base` with no version, one built rather than
    /// read, writes without a guard. Every part of `new` is written, so a part left at its default
    /// is cleared.
    ///
    /// # Errors
    ///
    /// - [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) when `new`
    ///   uses a part this account cannot hold, before anything is sent.
    /// - [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) when the reply
    ///   moved since `base` was read: read it again, and write again from that.
    /// - [`requires_confirmation`](ProviderError::requires_confirmation) when the request went out
    ///   and no answer came back, so the reply may have been written: read it before writing again.
    /// - Any other classified [`ProviderError`]: the reply was not written.
    async fn set_vacation(
        &self,
        account: &AccountId,
        base: &VacationResponse,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt>;
}

/// Refuses a reply that uses a part `vocabulary` cannot hold, naming each part.
///
/// Every adapter's [`Vacation::set_vacation`] calls this before sending, so the rule that a part
/// is refused rather than dropped has one implementation. Dropping one silently would change who
/// gets which reply: a reply limited to contacts written as a reply to everyone answers every
/// stranger who writes.
///
/// # Errors
///
/// [`ProviderError::invalid_state`] naming the parts, never their contents.
pub fn refuse_unheld_vacation(
    vocabulary: &VacationVocabulary,
    reply: &VacationResponse,
) -> ProviderResult<()> {
    let lost = vocabulary.lost_parts(reply);
    if lost.is_empty() {
        return Ok(());
    }
    let parts: Vec<String> = lost.iter().map(ToString::to_string).collect();
    Err(ProviderError::invalid_state(format!(
        "this account's out-of-office reply cannot hold its {}",
        parts.join(", ")
    )))
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;
