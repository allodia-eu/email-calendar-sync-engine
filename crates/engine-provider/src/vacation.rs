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
    version::VacationVersion,
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
    ///
    /// Asked per account, and possibly of the server, as
    /// [`MailImporter::mail_import_limits`](crate::import::MailImporter::mail_import_limits) is:
    /// what a Gmail account can hold depends on whether it is a Workspace account, and what a
    /// ManageSieve account can hold on the extensions its server lists.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the account cannot be asked.
    async fn vocabulary(&self, account: &AccountId) -> ProviderResult<VacationVocabulary>;

    /// Whether a write built on a stale read is refused ([`WriteGuard::Enforced`]) or silently
    /// wins ([`WriteGuard::Absent`]), as
    /// [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard) says for
    /// events.
    fn vacation_write_guard(&self) -> WriteGuard;

    /// The account's reply as the server holds it now, with the version a write built on it
    /// names.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the server cannot be asked or answers no reply.
    async fn read_vacation(&self, account: &AccountId) -> ProviderResult<VacationResponse>;

    /// Replaces the account's reply with `new`, under `precondition`.
    ///
    /// The caller says which write it means, at the call site:
    /// [`IfUnchanged`](VacationPrecondition::IfUnchanged) with the reply it read, refused if the
    /// reply moved since, or [`Unconditional`](VacationPrecondition::Unconditional) for a
    /// deliberate overwrite. The guard comes from `precondition` alone; `new`'s own
    /// [`version`](VacationResponse::version) and [`raw`](VacationResponse::raw) are not read.
    /// Every modelled part of `new` is written, so a part left at its default is cleared.
    ///
    /// # Errors
    ///
    /// Refused before anything is sent, so never written and never in doubt:
    ///
    /// - [`item_unavailable`](ProviderError::item_unavailable) (`Permanent`) when `new` uses a part
    ///   this account cannot hold, or its window runs at no instant ([`refuse_unheld_vacation`]):
    ///   this reply will never be written here as it stands.
    /// - [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent) when a guarded
    ///   write names a reply with no version on a transport that enforces one
    ///   ([`VacationPrecondition::version`]).
    ///
    /// After sending:
    ///
    /// - [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) when the reply
    ///   moved since it was read: read it again, and write again from that.
    /// - [`requires_confirmation`](ProviderError::requires_confirmation) when the request went out
    ///   and no answer came back, so the reply may have been written: read it before writing again.
    /// - Any other classified [`ProviderError`]: the reply was not written.
    async fn set_vacation(
        &self,
        account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt>;
}

/// What a write of the reply asks the server to verify before it stores anything: the
/// counterpart, for a setting, of [`WritePrecondition`](crate::WritePrecondition) for an event.
///
/// Two states the caller spells out, because a write that quietly drops its guard is the one
/// that loses a change: the other tab's edit, or a reply the user set on their phone a minute ago.
/// There is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VacationPrecondition<'a> {
    /// The reply must still be as the caller read it: its
    /// [`version`](VacationResponse::version) guards the write where the transport can
    /// ([`Vacation::vacation_write_guard`]), and its raw form supplies what the model does not
    /// carry.
    IfUnchanged(&'a VacationResponse),
    /// Nothing is checked: `new` lands over whatever the server holds. For a deliberate
    /// overwrite, such as an administrator turning a reply off, or a first write into a target
    /// nobody else edits.
    Unconditional,
}

impl VacationPrecondition<'_> {
    /// The version the write is guarded by: the read reply's where `guard` is
    /// [`Enforced`](WriteGuard::Enforced), and none for an unconditional write or on a transport
    /// that keeps no version. The one rule every adapter applies, so none of them turns a guarded
    /// write into an unguarded one.
    ///
    /// # Errors
    ///
    /// [`ProviderError::permanent`] for a guarded write on an enforcing transport whose reply
    /// carries no version: it was built rather than read, a caller's mistake that sending again
    /// cannot fix. Read the reply first, or write [`Unconditional`](Self::Unconditional) on
    /// purpose. Not `InvalidState`, which a host also sees on a write that may have landed.
    pub fn version(&self, guard: WriteGuard) -> ProviderResult<Option<&VacationVersion>> {
        match (self, guard) {
            (Self::IfUnchanged(base), WriteGuard::Enforced) => match &base.version {
                Some(version) => Ok(Some(version)),
                None => Err(ProviderError::permanent(
                    "a guarded write of the out-of-office reply needs the version it was read at; \
                     read it first, or write unconditionally",
                )),
            },
            _ => Ok(None),
        }
    }
}

/// Refuses a reply that cannot be written as it stands: one whose window runs at no instant, or
/// one that uses a part `vocabulary` cannot hold, naming each part.
///
/// Every adapter's [`Vacation::set_vacation`] calls this before sending, so the rule that a part
/// is refused rather than dropped has one implementation. Dropping one silently would change who
/// gets which reply: a reply limited to contacts written as a reply to everyone answers every
/// stranger who writes.
///
/// # Errors
///
/// [`ProviderError::item_unavailable`] naming the window or the parts, never their contents. It
/// is `Permanent` and flagged, so a host can tell "refused before sending, change the reply"
/// apart from a write that may have landed
/// ([`requires_confirmation`](ProviderError::requires_confirmation), `InvalidState`), which is
/// what a refusal by class alone could not do: the reply, like an import too large for its
/// target, will never be written here unchanged, and the account is otherwise unaffected.
pub fn refuse_unheld_vacation(
    vocabulary: &VacationVocabulary,
    reply: &VacationResponse,
) -> ProviderResult<()> {
    if reply.window.is_empty() {
        return Err(ProviderError::item_unavailable(
            "this out-of-office reply's window ends before it starts, so it would never be sent",
        ));
    }
    let lost = vocabulary.lost_parts(reply);
    if lost.is_empty() {
        return Ok(());
    }
    let parts: Vec<String> = lost.iter().map(ToString::to_string).collect();
    Err(ProviderError::item_unavailable(format!(
        "this account's out-of-office reply cannot hold its {}",
        parts.join(", ")
    )))
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;
