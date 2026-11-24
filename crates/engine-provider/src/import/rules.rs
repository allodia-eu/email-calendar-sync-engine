//! Importing an account's mail rules and its out-of-office reply: [`RulesImporter`], and the one
//! way every target does it ([`append_rules`], [`import_reply`]).
//!
//! An import is not an edit. It appends the rules carried from another account after the
//! target's own, never replacing or reordering them, under the import defaults of
//! [`plan_rules`]: a rule that sends mail out of the account arrives disabled unless the host
//! arms forwarding ([`ForwardingRules`]), and one naming a folder the host could not map loses
//! that action ([`MissingMailbox`](engine_core::rules::LossReason::MissingMailbox)). And it never
//! replaces a reply the target already holds.
//!
//! It writes through the target's own [`MailRules::replace_rules`] and
//! [`Vacation::set_vacation`], so which script holds the rules, how a write is guarded, whether
//! the server's reply keeps running beside them, and that a write never switches stored rules on
//! or off are each decided in one place, for an edit and an import alike
//! (`filters-and-out-of-office.md`, "Import").

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId},
    rules::{
        ForwardingRules, Kept, MailRule, PlanMode, RuleSet, RulesReceipt, RulesStored, plan_rules,
    },
    vacation::{RulesEffect, VacationReceipt, VacationResponse},
};

use crate::{
    MailRules, ProviderError, ProviderResult, Vacation, VacationPrecondition,
    refuse_unheld_vacation,
};

/// Imports mail rules and an out-of-office reply into a target account.
///
/// Reached through [`Provider::rules_importer`](crate::Provider::rules_importer), as the other
/// importers are. An adapter implements it with [`append_rules`] and [`import_reply`] over its own
/// [`MailRules`] and [`Vacation`], so every target imports the same way.
#[async_trait]
pub trait RulesImporter: Send + Sync {
    /// Appends `rules` after the target's own, never replacing or reordering them, and answers
    /// what each kept ([`append_rules`]).
    ///
    /// The host rewrites each [`RuleMailbox::Id`](engine_core::rules::RuleMailbox::Id) from
    /// source to target folder ids first, with the folder map its mail import built; an id it
    /// could not map stays, and that action is lost as `MissingMailbox`.
    ///
    /// The receipt's [`outcomes`](RulesReceipt::outcomes) are indexed by `rules`, as
    /// [`plan_rules`] decided them in [`Import`](PlanMode::Import) mode; it also says where the
    /// rules were stored ([`RulesStored`]), whether they run
    /// ([`activation`](RulesReceipt::activation): a script stored but switched off stays off,
    /// and the imported rules with it), and what the write did to a running reply.
    ///
    /// # Errors
    ///
    /// As [`MailImporter`](super::MailImporter)'s:
    /// [`requires_confirmation`](ProviderError::requires_confirmation) means the rules may have
    /// been stored, so read them before importing again;
    /// [`item_unavailable`](ProviderError::item_unavailable) means they never will be as they stand
    /// (among them, a target whose own rules a write would not carry back unchanged).
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict): the target's rules
    /// moved between the import's read and its write; nothing was written, so import again.
    async fn import_rules(
        &self,
        account: &AccountId,
        rules: &[MailRule],
        forwarding: ForwardingRules,
    ) -> ProviderResult<RulesReceipt>;

    /// Writes `reply` where the target holds none of its own ([`import_reply`]).
    ///
    /// # Errors
    ///
    /// [`item_unavailable`](ProviderError::item_unavailable) when the target already holds a
    /// different reply, or `reply` uses a part the target cannot hold; the target's own
    /// [`Vacation::set_vacation`] errors otherwise.
    async fn import_vacation(
        &self,
        account: &AccountId,
        reply: &VacationResponse,
    ) -> ProviderResult<VacationReceipt>;
}

/// Appends `rules` after `target`'s own and answers what each kept: the one import every
/// [`RulesImporter`] runs.
///
/// 1. Reads the target's rules and plans `rules` against its vocabulary in
///    [`Import`](PlanMode::Import) mode, with `mailboxes`, the target folders a rule may file into
///    by id ([`RuleVocabulary::with_mailboxes`](engine_core::rules::RuleVocabulary::with_mailboxes)).
/// 2. Refuses before writing where the target's own edit would not write its own rules back as they
///    are: an import never changes them.
/// 3. Writes the target's rules, unchanged, then the planned ones, through
///    [`MailRules::replace_rules`] built on that read, so it is guarded as an edit is. Where no
///    rule is left to add, nothing is written.
///
/// # Errors
///
/// [`RulesImporter::import_rules`]'s.
pub async fn append_rules(
    target: &dyn MailRules,
    account: &AccountId,
    rules: &[MailRule],
    forwarding: ForwardingRules,
    mailboxes: &BTreeSet<MailboxId>,
) -> ProviderResult<RulesReceipt> {
    let vocabulary = target.rule_vocabulary(account).await?;
    let base = target.read_rules(account).await?;
    refuse_changed_own(&base, &plan_rules(&base.rules, &vocabulary, PlanMode::Edit))?;
    let into = vocabulary.with_mailboxes(mailboxes.iter().cloned());
    let plan = plan_rules(rules, &into, PlanMode::Import { forwarding });
    let added = plan.written();
    if added.is_empty() {
        let receipt = RulesReceipt::new(base.version.clone(), plan.outcomes)
            .with_stored(RulesStored::InPlace)
            .with_activation(base.activation);
        return Ok(receipt);
    }
    let mut all = base.rules.clone();
    all.extend(added);
    let mut receipt = target.replace_rules(account, &base, &all).await?;
    receipt.outcomes = plan.outcomes;
    Ok(receipt)
}

/// Refuses an import where the target's own edit, planning `base`'s rules as `own`, would write
/// any of them differently or not at all: written through it, they would change.
fn refuse_changed_own(base: &RuleSet, own: &engine_core::rules::RulesPlan) -> ProviderResult<()> {
    let unchanged = own.written() == base.rules
        && own
            .outcomes
            .iter()
            .all(|outcome| outcome.kept != Kept::NotKept);
    if unchanged {
        return Ok(());
    }
    Err(ProviderError::item_unavailable(
        "the target's own mail rules would not be written back as they are, so nothing is \
         imported beside them",
    ))
}

/// Writes `reply` into `target` where it holds no reply of its own: the one reply import every
/// [`RulesImporter`] runs.
///
/// - A part the target cannot hold refuses the reply whole ([`refuse_unheld_vacation`], as an edit
///   is refused): dropping a window, an audience or an outside reply would widen who is answered.
/// - Where the target's reply is empty (off, with nothing to say and no window) it is written,
///   guarded by that read.
/// - Where it already says what `reply` says, nothing is written, so an import run again after an
///   interruption answers as the first did.
/// - Any other reply, on or off, is the target's own: refused, never replaced. A host that means to
///   replace it writes through [`Vacation::set_vacation`] itself.
///
/// # Errors
///
/// [`RulesImporter::import_vacation`]'s.
pub async fn import_reply(
    target: &dyn Vacation,
    account: &AccountId,
    reply: &VacationResponse,
) -> ProviderResult<VacationReceipt> {
    refuse_unheld_vacation(&target.vocabulary(account).await?, reply)?;
    let held = target.read_vacation(account).await?;
    if says(&held) == says(reply) {
        return Ok(VacationReceipt::new(held.version, RulesEffect::Untouched));
    }
    if says(&held) != says(&VacationResponse::default()) {
        return Err(ProviderError::item_unavailable(
            "the target already holds an out-of-office reply of its own, so the imported one is \
             not written over it",
        ));
    }
    target
        .set_vacation(account, VacationPrecondition::IfUnchanged(&held), reply)
        .await
}

/// What a reply says: every modelled part, without the version and the raw form it was read with.
fn says(reply: &VacationResponse) -> VacationResponse {
    let mut said = VacationResponse::default()
        .with_window(reply.window)
        .with_reply(reply.reply.clone())
        .with_audience(reply.audience);
    if let Some(outside) = &reply.outside_reply {
        said = said.with_outside_reply(outside.clone());
    }
    if reply.enabled {
        said.turned_on()
    } else {
        said
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
