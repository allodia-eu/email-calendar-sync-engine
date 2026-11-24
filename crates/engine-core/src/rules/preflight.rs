//! What a transfer host lists before it imports: the rules an import holds back because they send
//! mail out of the account.

use super::{ForwardingRules, MailRule, PlanMode, RuleOutcome, RuleVocabulary, plan_rules};

/// Each rule of `rules` an import into `target` holds back because it sends mail out of the
/// account (a redirect, a forward, a provider's own action that sends), with its outcome under
/// the default ([`ForwardingRules::Disabled`]): [`Disabled`](super::Kept::Disabled), stored for an
/// operator to review, or [`NotKept`](super::Kept::NotKept) where the target cannot store it
/// disabled (`filters-and-out-of-office.md`, decision 2).
///
/// These are exactly the rules the forwarding switch decides: those [`plan_rules`] keeps
/// differently with [`ForwardingRules::Enabled`]. It is decided by `plan_rules` alone, so the list
/// agrees with the receipt of an import planned against the same `target`; an import plans
/// against the vocabulary and folders it reads when it runs, so a target that changed in between
/// can answer differently, and the receipt is what happened. A rule already disabled at the
/// source, one its source marks in error (stored disabled either way, its outcome saying
/// [`SourceInError`](super::LossReason::SourceInError)), or one the target would not keep either
/// way, is not listed: the switch changes nothing for it.
#[must_use]
pub fn held_forwarding(rules: &[MailRule], target: &RuleVocabulary) -> Vec<RuleOutcome> {
    let import = |forwarding| plan_rules(rules, target, PlanMode::Import { forwarding });
    let held = import(ForwardingRules::Disabled);
    let armed = import(ForwardingRules::Enabled);
    held.outcomes
        .into_iter()
        .zip(armed.outcomes)
        .filter(|(held, armed)| held.kept != armed.kept)
        .map(|(held, _)| held)
        .collect()
}
