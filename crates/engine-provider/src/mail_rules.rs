//! An account's mail rules as a capability: [`MailRules`], reached through
//! [`Provider::mail_rules`](crate::Provider::mail_rules).
//!
//! Rules are an account setting, not messages, so every host writes them directly, guarded by
//! the version it read, and never through the outbox; the store keeps no copy
//! (`filters-and-out-of-office.md`, decision 1). The set is replaced whole, because order is a
//! property of the set and two transports store the set as one script.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    rules::{MailRule, RuleSet, RuleVocabulary, RulesReceipt},
    version::RuleSetVersion,
};

use crate::{ProviderError, ProviderResult, WriteGuard};

/// An account's mail rules: what they can say, read them, and replace them.
///
/// An adapter that can implements this and answers `Some(self)` from
/// [`Provider::mail_rules`](crate::Provider::mail_rules) only for an account that has rules, or
/// may have them while the adapter has not yet found out (an IMAP account's first ManageSieve
/// probe, during which a call waits for the answer): `Some` is the capability check, and a call
/// refused as described there says it turned out missing.
#[async_trait]
pub trait MailRules: Send + Sync {
    /// What this account's rule language can express, for
    /// [`plan_rules`](engine_core::rules::plan_rules): a host greys out what it cannot hold.
    ///
    /// Asked per account, as [`Vacation::vocabulary`](crate::Vacation::vocabulary) is: two Sieve
    /// servers list different extensions.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the account cannot be asked.
    async fn rule_vocabulary(&self, account: &AccountId) -> ProviderResult<RuleVocabulary>;

    /// Whether a write built on a stale read is refused ([`WriteGuard::Enforced`]) or silently
    /// wins ([`WriteGuard::Absent`]).
    fn rules_write_guard(&self) -> WriteGuard;

    /// The account's rules as the server holds them now, with the version a write built on
    /// them names, and the provider's own form a write copies unchanged rules from.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the server cannot be asked.
    async fn read_rules(&self, account: &AccountId) -> ProviderResult<RuleSet>;

    /// Replaces the account's rules with `rules`, built on `base`, the set as the caller read
    /// it: its version guards the write where the transport can
    /// ([`rules_write_guard`](Self::rules_write_guard)), and its raw form supplies every rule
    /// the caller did not change, byte for byte.
    ///
    /// What the account cannot hold is decided first, by
    /// [`plan_rules`](engine_core::rules::plan_rules) in
    /// [`Edit`](engine_core::rules::PlanMode::Edit) mode, and the receipt carries its outcome per
    /// rule: a rule the account cannot hold is not written, and the receipt says so.
    ///
    /// The rules are written where they are, running or not as they were
    /// ([`RuleSet::activation`]): a write never switches stored rules on or off. Only an account
    /// with no rules to edit gets new ones that run
    /// ([`RulesStored::CreatedRunning`](engine_core::rules::RulesStored::CreatedRunning)), and the
    /// receipt says what that did to a running reply
    /// ([`ReplyEffect`](engine_core::rules::ReplyEffect)).
    ///
    /// The guard is `base`'s version and nothing else: there is no unconditional write, unlike
    /// [`Vacation::set_vacation`](crate::Vacation::set_vacation)'s, because no host has needed a
    /// deliberate overwrite of a rule set yet, and writing a script over one nobody read is how a
    /// user's hand-written rules are lost. One is an addition when a host needs it.
    ///
    /// # Errors
    ///
    /// Refused before anything is sent, so never written:
    ///
    /// - [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent) when `base`
    ///   carries no version on a transport that enforces one ([`guard_version`]).
    /// - [`item_unavailable`](ProviderError::item_unavailable) when the rules cannot be written as
    ///   they stand: the script the account holds cannot be edited rule by rule, or the written
    ///   rules exceed the account's size limit.
    ///
    /// After sending:
    ///
    /// - [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) when the rules
    ///   moved since `base` was read: read them again, and write again from that.
    /// - [`item_unavailable`](ProviderError::item_unavailable) when the server refused the written
    ///   rules as invalid or too large; nothing was stored.
    /// - [`refused_by_target`](ProviderError::refused_by_target) with
    ///   [`StorageFull`](crate::TargetRefusal::StorageFull) when the account has no room for the
    ///   rules (JMAP's `overQuota`); waiting does not clear it.
    /// - [`requires_confirmation`](ProviderError::requires_confirmation) when the request went out
    ///   and no answer came back, so the rules may have been written: read them before writing
    ///   again.
    async fn replace_rules(
        &self,
        account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt>;
}

/// The version a write built on `base` is guarded by: `base`'s where `guard` is
/// [`Enforced`](WriteGuard::Enforced), and none on a transport that keeps none. The one rule every
/// adapter applies, so none turns a guarded write into an unguarded one.
///
/// # Errors
///
/// [`ProviderError::permanent`] on an enforcing transport when `base` carries no version: it
/// was built rather than read, a caller's mistake that sending again cannot fix.
pub fn guard_version(base: &RuleSet, guard: WriteGuard) -> ProviderResult<Option<&RuleSetVersion>> {
    match (guard, &base.version) {
        (WriteGuard::Enforced, Some(version)) => Ok(Some(version)),
        (WriteGuard::Enforced, None) => Err(ProviderError::permanent(
            "a write of the mail rules needs the version they were read at; read them first",
        )),
        _ => Ok(None),
    }
}

#[cfg(test)]
#[path = "mail_rules_tests.rs"]
mod tests;
