//! Importing an account's mail rules and its out-of-office reply: [`RulesImporter`], and the one
//! way every target does it ([`append_rules`], [`import_reply`](super::import_reply)).
//!
//! An import is not an edit. It appends the rules carried from another account after the
//! target's own, never replacing or reordering them, under the import defaults of
//! [`plan_rules`]: a rule that sends mail out of the account arrives disabled unless the host
//! arms forwarding ([`ForwardingRules`]), and one naming a folder the host's [`FolderMap`] does
//! not hold loses that action ([`MissingMailbox`](engine_core::rules::LossReason::MissingMailbox)).
//! A rule the target already holds is not written again, so an import can be run again. And it
//! never replaces a reply the target already holds.
//!
//! It writes through the target's own [`MailRules::replace_rules`] and
//! [`Vacation::set_vacation`](crate::Vacation::set_vacation), so which script holds the rules, how
//! a write is guarded, whether the server's reply keeps running beside them, and that a write never
//! switches stored rules on or off are each decided in one place, for an edit and an import alike
//! (`filters-and-out-of-office.md`, "Import").

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    rules::{
        FolderMap, ForwardingRules, Kept, MailRule, PlanMode, RuleBody, RuleSet, RulesPlan,
        RulesReceipt, RulesStored, plan_rules,
    },
    vacation::{VacationReceipt, VacationResponse},
};

use crate::{MailRules, ProviderError, ProviderResult};

/// Imports mail rules and an out-of-office reply into a target account.
///
/// Reached through [`Provider::rules_importer`](crate::Provider::rules_importer), as the other
/// importers are. An adapter implements it with [`append_rules`] and
/// [`import_reply`](super::import_reply) over its own [`MailRules`] and
/// [`Vacation`](crate::Vacation), so every target imports the same way.
#[async_trait]
pub trait RulesImporter: Send + Sync {
    /// Appends `rules` after the target's own, never replacing or reordering them, and answers
    /// what each kept ([`append_rules`]).
    ///
    /// `folders` is the folder map the host's mail import built: each
    /// [`RuleMailbox::Id`](engine_core::rules::RuleMailbox::Id), a source folder id, is written as
    /// the target id it maps to. One the map does not hold, or whose target folder the target no
    /// longer has, loses that action as `MissingMailbox`.
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
        folders: &FolderMap,
    ) -> ProviderResult<RulesReceipt>;

    /// Writes `reply` where the target holds none of its own
    /// ([`import_reply`](super::import_reply)).
    ///
    /// # Errors
    ///
    /// [`item_unavailable`](ProviderError::item_unavailable) when the target already holds a
    /// different reply, or `reply` uses a part the target cannot hold; the target's own
    /// [`Vacation::set_vacation`](crate::Vacation::set_vacation) errors otherwise.
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
///    [`Import`](PlanMode::Import) mode, filing by id through `folders`
///    ([`RuleVocabulary::with_folders`](engine_core::rules::RuleVocabulary::with_folders)). An
///    importer that can see which target folders exist hands in only those.
/// 2. Refuses before writing where the target's own edit would not write its own rules back as they
///    are: an import never changes them.
/// 3. Leaves out each planned rule the target already holds an equal typed rule for, one held rule
///    for each (so two equal rules handed in need two held), and says so
///    ([`RulesReceipt::already_held`]). A rule equal to one the user wrote themselves counts as
///    imported too: written again, it would do nothing the held one does not already do, since
///    rules apply in order and the first copy acts on the message first.
/// 4. Writes the target's rules, unchanged, then the rules left, through
///    [`MailRules::replace_rules`] built on that read, so it is guarded as an edit is. Each is
///    written as a rule of no script ([`MailRule::origin`] cleared): what it was read from is the
///    source's, and a target script with the same text must not be taken for it. Where no rule is
///    left to add, nothing is written ([`RulesStored::Unwritten`]).
///
/// # Errors
///
/// [`RulesImporter::import_rules`]'s.
pub async fn append_rules(
    target: &dyn MailRules,
    account: &AccountId,
    rules: &[MailRule],
    forwarding: ForwardingRules,
    folders: &FolderMap,
) -> ProviderResult<RulesReceipt> {
    let vocabulary = target.rule_vocabulary(account).await?;
    let base = target.read_rules(account).await?;
    refuse_changed_own(&base, &plan_rules(&base.rules, &vocabulary, PlanMode::Edit))?;
    let into = vocabulary.with_folders(folders.clone());
    let plan = plan_rules(rules, &into, PlanMode::Import { forwarding });
    let (added, already_held) = not_held(&base, &plan);
    if added.is_empty() {
        let receipt = RulesReceipt::new(base.version.clone(), plan.outcomes)
            .with_stored(RulesStored::Unwritten)
            .with_activation(base.activation)
            .with_already_held(already_held);
        return Ok(receipt);
    }
    let mut all = base.rules.clone();
    all.extend(added);
    let mut receipt = target.replace_rules(account, &base, &all).await?;
    receipt.outcomes = plan.outcomes;
    Ok(receipt.with_already_held(already_held))
}

/// The planned rules `base` does not already hold, each as a rule of no script, and the indices of
/// those it does: an equal typed rule held counts for one planned rule only.
fn not_held(base: &RuleSet, plan: &RulesPlan) -> (Vec<MailRule>, Vec<usize>) {
    let unplaced = |rule: &MailRule| {
        let mut rule = rule.clone();
        rule.origin = None;
        rule
    };
    let mut held: Vec<MailRule> = base
        .rules
        .iter()
        .filter(|rule| matches!(rule.body, RuleBody::Typed(_)))
        .map(unplaced)
        .collect();
    let mut added = Vec::new();
    let mut already = Vec::new();
    for planned in &plan.rules {
        let rule = unplaced(&planned.rule);
        let found = matches!(rule.body, RuleBody::Typed(_))
            .then(|| held.iter().position(|held| same(held, &rule)))
            .flatten();
        match found {
            Some(at) => {
                held.swap_remove(at);
                already.push(planned.source);
            }
            None => added.push(rule),
        }
    }
    (added, already)
}

/// Whether two typed rules are the same rule: equal in every part, their actions in any order,
/// since the order of a rule's actions means nothing (a Sieve writer puts marks before filing,
/// and the rule reads back so).
fn same(a: &MailRule, b: &MailRule) -> bool {
    let (mut a, mut b) = (a.clone(), b.clone());
    let (RuleBody::Typed(left), RuleBody::Typed(right)) = (&mut a.body, &mut b.body) else {
        return false;
    };
    let mut right_actions = std::mem::take(&mut right.actions);
    let left_actions = std::mem::take(&mut left.actions);
    let all_matched = left_actions.len() == right_actions.len()
        && left_actions.iter().all(|action| {
            right_actions
                .iter()
                .position(|other| other == action)
                .map(|at| right_actions.swap_remove(at))
                .is_some()
        });
    all_matched && a == b
}

/// Refuses an import where the target's own edit, planning `base`'s rules as `own`, would write
/// any of them differently or not at all: written through it, they would change.
fn refuse_changed_own(base: &RuleSet, own: &RulesPlan) -> ProviderResult<()> {
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

#[cfg(test)]
#[path = "rules_tests.rs"]
pub(super) mod tests;
