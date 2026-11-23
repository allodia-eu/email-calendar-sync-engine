//! What a target keeps of a rule set, decided before anything is sent.

use super::{
    Action, Condition, HeaderField, MailRule, MatchMode, RuleBody, RuleFeature, RuleMailbox,
    RuleVocabulary, Support, TypedRule,
};
use crate::mail::{Keyword, MailboxRole, SystemKeyword};

/// Why rules are being planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanMode {
    /// The user's own edit, written as given.
    Edit,
    /// Rules carried from another account, under the import defaults.
    Import {
        /// Whether rules that send mail out of the account arrive running.
        forwarding: ForwardingRules,
    },
}

/// Whether imported rules that send mail out of the account (redirect, forward) arrive running.
///
/// Off by default: planted forwarding rules are a known way an attacker keeps reading a
/// compromised mailbox, and a migration should not quietly arm one on a new server
/// (`filters-and-out-of-office.md`, decision 2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForwardingRules {
    /// Imported disabled, for an operator to review.
    #[default]
    Disabled,
    /// Imported as they ran.
    Enabled,
}

/// What a target will be sent, and what became of each rule handed in.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "a plan says which rules the target would lose"]
pub struct RulesPlan {
    /// The rules as they will be written, in order: each kept rule reduced to what the target
    /// holds. A rule [`NotKept`](Kept::NotKept) is not among them.
    pub rules: Vec<MailRule>,
    /// One outcome per rule handed in, in the same order.
    pub outcomes: Vec<RuleOutcome>,
}

/// What became of one rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuleOutcome {
    /// The rule's index in the rules handed in.
    pub source: usize,
    /// How much of it the target keeps.
    pub kept: Kept,
    /// Each part lost or changed, and why.
    pub losses: Vec<Loss>,
}

/// How much of a rule a target keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kept {
    /// All of it, some parts perhaps [`Approximated`](LossReason::Approximated).
    Whole,
    /// Without an action, its stop, its name or its order: it does less, never more.
    Reduced,
    /// All of it, stored disabled for review (an imported forwarding rule).
    Disabled,
    /// None of it: what was lost would have widened what it matches, or left it nothing to do.
    NotKept,
}

/// One part of a rule lost or changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Loss {
    /// Which part.
    pub part: RulePart,
    /// Why.
    pub reason: LossReason,
}

/// A part of a rule, by position: never its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RulePart {
    /// The whole rule.
    Rule,
    /// How its conditions combine.
    Mode,
    /// Its condition at this index.
    Condition(usize),
    /// Its exception at this index.
    Exception(usize),
    /// Its action at this index.
    Action(usize),
    /// Its stop.
    Stop,
    /// Whether it runs.
    Enabled,
    /// Its name.
    Name,
    /// Its place in the order.
    Order,
}

/// Why a part was lost or changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LossReason {
    /// The target's language has no equivalent.
    NotExpressible,
    /// Carried with a stated difference.
    Approximated,
    /// The folder it names has no counterpart on the target.
    MissingMailbox,
    /// The target refuses to send mail on there: too many addresses, an address it has not
    /// verified, or a forwarding rule an import arms only on request.
    ForwardNotAllowed,
    /// Over a limit of the target: the rule count, the script size.
    LimitExceeded,
    /// The source itself marks the rule as in error (Graph `hasError`).
    SourceInError,
    /// A rule the engine could not read, going to another kind of transport.
    OpaqueSource,
}

/// Decides what `target` keeps of `rules`, and how each rule is to be written.
///
/// One function for every caller, so the receipt's classes come from one place: a settings
/// screen greys out what the account cannot hold, a transfer host's preflight reports without
/// touching the target, and every adapter's write runs it first and adds only what the server
/// alone can refuse (`filters-and-out-of-office.md`, "The pure core"). Two safety rules are part
/// of it, not host policy:
///
/// - **A lost condition or exception drops the rule** ([`Kept::NotKept`]): either would widen what
///   it matches, so "delete newsletters except from my bank" never deletes the bank's mail. So does
///   a lost way of combining conditions, and losing every action. A lost action, stop or name
///   narrows what the rule does, so the rule is [`Kept::Reduced`].
/// - **An imported rule that sends mail out of the account is stored disabled**
///   ([`Kept::Disabled`]) unless the import says [`ForwardingRules::Enabled`], and not kept where
///   the target cannot store a disabled rule. A user's own edit ([`PlanMode::Edit`]) is written as
///   given.
pub fn plan_rules(rules: &[MailRule], target: &RuleVocabulary, mode: PlanMode) -> RulesPlan {
    let ordered = rules.len() > 1;
    let mut plan = RulesPlan {
        rules: Vec::new(),
        outcomes: Vec::with_capacity(rules.len()),
    };
    for (source, rule) in rules.iter().enumerate() {
        let mut planned = Planned::default();
        let written = planned.rule(rule, target, mode, ordered);
        if !planned.fatal {
            plan.rules.push(written);
        }
        plan.outcomes.push(planned.outcome(source));
    }
    plan
}

/// What planning one rule found so far.
#[derive(Default)]
struct Planned {
    losses: Vec<Loss>,
    fatal: bool,
    disabled_for_review: bool,
}

impl Planned {
    fn lose(&mut self, part: RulePart, reason: LossReason) {
        self.losses.push(Loss { part, reason });
    }

    /// Records what `support` says of a part the rule cannot do without; `true` where kept.
    fn require(&mut self, part: RulePart, support: Option<Support>) -> bool {
        match support {
            Some(Support::Exact) => true,
            Some(Support::Approximated) => {
                self.lose(part, LossReason::Approximated);
                true
            }
            None => {
                self.lose(part, LossReason::NotExpressible);
                self.fatal = true;
                false
            }
        }
    }

    /// Records what `support` says of a part the rule can do without; `true` where kept.
    fn optional(&mut self, part: RulePart, support: Option<Support>) -> bool {
        match support {
            Some(Support::Approximated) => {
                self.lose(part, LossReason::Approximated);
                true
            }
            Some(Support::Exact) => true,
            None => {
                self.lose(part, LossReason::NotExpressible);
                false
            }
        }
    }

    fn rule(
        &mut self,
        rule: &MailRule,
        target: &RuleVocabulary,
        mode: PlanMode,
        ordered: bool,
    ) -> MailRule {
        let mut written = rule.clone();
        match &mut written.body {
            RuleBody::Opaque(raw) => {
                if !target.stores_opaque(raw.syntax()) {
                    self.lose(RulePart::Rule, LossReason::OpaqueSource);
                    self.fatal = true;
                }
            }
            RuleBody::Typed(typed) => self.typed(typed, target),
        }
        if written.name.is_some()
            && !self.optional(RulePart::Name, target.support(RuleFeature::Name))
        {
            written.name = None;
        }
        if ordered {
            self.optional(RulePart::Order, target.support(RuleFeature::Order));
        }
        if !written.enabled {
            self.require(RulePart::Enabled, target.support(RuleFeature::Disabled));
        }
        let forwards =
            matches!(&written.body, RuleBody::Typed(typed) if typed.actions.iter().any(sends_out));
        if written.enabled
            && forwards
            && mode
                == (PlanMode::Import {
                    forwarding: ForwardingRules::Disabled,
                })
        {
            if target.support(RuleFeature::Disabled).is_some() {
                written.enabled = false;
                self.disabled_for_review = true;
            } else {
                self.lose(RulePart::Enabled, LossReason::ForwardNotAllowed);
                self.fatal = true;
            }
        }
        written
    }

    fn typed(&mut self, rule: &mut TypedRule, target: &RuleVocabulary) {
        let when = &rule.when;
        if when.mode == MatchMode::Any && when.conditions.len() > 1 {
            self.require(RulePart::Mode, target.support(RuleFeature::MatchAny));
        }
        for (i, condition) in when.conditions.iter().enumerate() {
            self.require(RulePart::Condition(i), condition_support(condition, target));
        }
        for (i, exception) in when.except.iter().enumerate() {
            let support = target
                .support(RuleFeature::Except)
                .and(condition_support(exception, target));
            self.require(RulePart::Exception(i), support);
        }
        let had_actions = !rule.actions.is_empty();
        let mut kept = Vec::with_capacity(rule.actions.len());
        for (i, action) in rule.actions.iter().enumerate() {
            let too_many = match action {
                Action::Redirect { to, .. } | Action::Forward { to } => {
                    target.max_redirects().is_some_and(|max| to.len() > max)
                }
                _ => false,
            };
            if too_many {
                self.lose(RulePart::Action(i), LossReason::ForwardNotAllowed);
            } else if self.optional(RulePart::Action(i), action_support(action, target)) {
                kept.push(action.clone());
            }
        }
        if had_actions && kept.is_empty() {
            self.fatal = true;
        }
        rule.actions = kept;
        if rule.stop && !self.optional(RulePart::Stop, target.support(RuleFeature::Stop)) {
            rule.stop = false;
        }
    }

    fn outcome(self, source: usize) -> RuleOutcome {
        let kept = if self.fatal {
            Kept::NotKept
        } else if self.disabled_for_review {
            Kept::Disabled
        } else if self
            .losses
            .iter()
            .any(|loss| loss.reason != LossReason::Approximated)
        {
            Kept::Reduced
        } else {
            Kept::Whole
        };
        RuleOutcome {
            source,
            kept,
            losses: self.losses,
        }
    }
}

/// Whether `action` sends mail out of the account.
fn sends_out(action: &Action) -> bool {
    matches!(action, Action::Redirect { .. } | Action::Forward { .. })
}

/// The worst support among `features`: missing wins, then approximated.
fn worst(target: &RuleVocabulary, features: &[RuleFeature]) -> Option<Support> {
    let mut worst = Support::Exact;
    for feature in features {
        match target.support(*feature)? {
            Support::Approximated => worst = Support::Approximated,
            Support::Exact => {}
        }
    }
    Some(worst)
}

/// How `target` holds `condition`.
fn condition_support(condition: &Condition, target: &RuleVocabulary) -> Option<Support> {
    use super::AddressPart;
    match condition {
        Condition::Header { field, test } => match field {
            HeaderField::Subject => worst(target, &[RuleFeature::SubjectText(test.op)]),
            HeaderField::Named(_) => worst(target, &[RuleFeature::HeaderText(test.op)]),
        },
        Condition::Address { part, test, .. } => {
            let text = RuleFeature::AddressText(test.op);
            match part {
                AddressPart::All => worst(target, &[text]),
                AddressPart::LocalPart => worst(target, &[text, RuleFeature::AddressLocalPart]),
                AddressPart::Domain => worst(target, &[text, RuleFeature::AddressDomain]),
            }
        }
        Condition::SentToMe(_) => worst(target, &[RuleFeature::SentToMe]),
        Condition::Body(test) => worst(target, &[RuleFeature::Body(test.op)]),
        Condition::Size(super::SizeBound::Over(_)) => worst(target, &[RuleFeature::SizeOver]),
        Condition::Size(super::SizeBound::Under(_)) => worst(target, &[RuleFeature::SizeUnder]),
        Condition::HasAttachment => worst(target, &[RuleFeature::HasAttachment]),
        Condition::Importance(_) => worst(target, &[RuleFeature::Importance]),
        Condition::Native(native) => target.stores_native(native.kind).then_some(Support::Exact),
    }
}

/// How `target` holds `action`.
fn action_support(action: &Action, target: &RuleVocabulary) -> Option<Support> {
    match action {
        Action::FileInto { mailbox, copy } => {
            let into = match mailbox {
                // The inbox needs no role to be named: every transport has one by name.
                RuleMailbox::Role(MailboxRole::Inbox) | RuleMailbox::Path(_) => {
                    RuleFeature::FileInto
                }
                RuleMailbox::Role(_) => RuleFeature::FileIntoRole,
                RuleMailbox::Id(_) => RuleFeature::FileIntoId,
            };
            if *copy {
                worst(target, &[into, RuleFeature::FileIntoCopy])
            } else {
                worst(target, &[into])
            }
        }
        Action::Discard => worst(target, &[RuleFeature::Discard]),
        Action::Redirect { copy: false, .. } => worst(target, &[RuleFeature::Redirect]),
        Action::Redirect { copy: true, .. } => {
            worst(target, &[RuleFeature::Redirect, RuleFeature::RedirectCopy])
        }
        Action::Forward { .. } => worst(target, &[RuleFeature::Forward]),
        Action::AddKeywords(keywords) => {
            let features: Vec<RuleFeature> = keywords.iter().map(keyword_feature).collect();
            worst(target, &features)
        }
        Action::MarkImportance(_) => worst(target, &[RuleFeature::MarkImportance]),
        Action::Native(native) => target.stores_native(native.kind).then_some(Support::Exact),
    }
}

/// The feature adding `keyword` needs.
fn keyword_feature(keyword: &Keyword) -> RuleFeature {
    match keyword.as_system() {
        Some(SystemKeyword::Seen) => RuleFeature::MarkRead,
        Some(SystemKeyword::Flagged) => RuleFeature::Flag,
        _ => RuleFeature::NamedKeyword,
    }
}
