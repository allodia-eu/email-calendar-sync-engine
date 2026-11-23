//! What a target keeps of a rule set, decided before anything is sent.

use super::{
    Action, Condition, MailRule, MatchMode, RuleBody, RuleFeature, RuleMailbox, RuleVocabulary,
    Support, TypedRule,
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
    /// holds, with its index in the rules handed in. A rule [`NotKept`](Kept::NotKept) is not
    /// among them.
    pub rules: Vec<PlannedRule>,
    /// One outcome per rule handed in, in the same order.
    pub outcomes: Vec<RuleOutcome>,
}

/// One rule as it will be written, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlannedRule {
    /// The rule's index in the rules handed in.
    pub source: usize,
    /// The rule as it will be written.
    pub rule: MailRule,
}

impl RulesPlan {
    /// The rules as they will be written, without their sources: what a writer takes.
    #[must_use]
    pub fn written(&self) -> Vec<MailRule> {
        self.rules
            .iter()
            .map(|planned| planned.rule.clone())
            .collect()
    }
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
    /// Stored disabled for review: an imported rule that sends mail out of the account. What
    /// else it lost is in its [`losses`](RuleOutcome::losses), as for any rule.
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
///
/// A rule carried whole keeps its text, so it keeps the name its text holds and stays enabled:
/// renaming it is a lost name, and disabling it is not expressible. It goes only to a target that
/// speaks its language and lists every extension it requires; and an import that arms no
/// forwarding rule does not carry one that may send mail, since its text cannot be disabled.
///
/// The redirect limit ([`RuleVocabulary::max_redirects`]) is checked per rule, over all its
/// actions' addresses. It bounds one message's delivery, so two rules that each fit can together
/// exceed it for a message both match; the plan cannot see that, and the server enforces it when
/// the message arrives.
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
            plan.rules.push(PlannedRule {
                source,
                rule: written,
            });
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
                if !target.stores_opaque(raw) {
                    self.lose(RulePart::Rule, LossReason::OpaqueSource);
                    self.fatal = true;
                }
                // Its text holds its name and runs it: neither can change while it is carried.
                if written.name.as_ref() != raw.name() {
                    self.lose(RulePart::Name, LossReason::NotExpressible);
                    written.name = raw.name().cloned();
                }
                if !written.enabled {
                    self.lose(RulePart::Enabled, LossReason::NotExpressible);
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
        if !written.enabled && matches!(written.body, RuleBody::Typed(_)) {
            self.require(RulePart::Enabled, target.support(RuleFeature::Disabled));
        }
        let arming = mode
            == (PlanMode::Import {
                forwarding: ForwardingRules::Disabled,
            });
        if arming
            && written.enabled
            && matches!(&written.body, RuleBody::Opaque(raw) if raw.sends_mail())
        {
            // Text that may send mail cannot be stored disabled: an import that arms no
            // forwarding rule does not carry it at all.
            self.lose(RulePart::Rule, LossReason::ForwardNotAllowed);
            self.fatal = true;
        }
        let forwards =
            matches!(&written.body, RuleBody::Typed(typed) if typed.actions.iter().any(sends_out));
        if written.enabled && forwards && arming {
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
            let support = both(
                target.support(RuleFeature::Except),
                condition_support(exception, target),
            );
            self.require(RulePart::Exception(i), support);
        }
        let had_actions = !rule.actions.is_empty();
        let mut kept = Vec::with_capacity(rule.actions.len());
        // The redirect limit bounds one message's delivery, so the addresses of every action of
        // the rule count against it together, kept in order while they fit.
        let mut redirects = 0;
        for (i, action) in rule.actions.iter().enumerate() {
            let addresses = match action {
                Action::Redirect { to, .. } | Action::Forward { to } => to.len(),
                _ => 0,
            };
            if target
                .max_redirects()
                .is_some_and(|max| redirects + addresses > max)
            {
                self.lose(RulePart::Action(i), LossReason::ForwardNotAllowed);
            } else if self.optional(RulePart::Action(i), action_support(action, target)) {
                redirects += addresses;
                kept.push(action.clone());
            }
        }
        if had_actions && kept.is_empty() {
            self.fatal = true;
        }
        rule.actions = kept;
        if rule.stop && !self.optional(RulePart::Stop, target.support(RuleFeature::Stop)) {
            rule.stop = false;
            // A rule whose one effect was to stop the rules after it does nothing without it.
            if rule.actions.is_empty() {
                self.fatal = true;
            }
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
    match action {
        Action::Redirect { .. } | Action::Forward { .. } => true,
        Action::Native(native) => native.sends_mail,
        _ => false,
    }
}

/// The worse of two supports: missing wins, then approximated.
fn both(a: Option<Support>, b: Option<Support>) -> Option<Support> {
    match (a?, b?) {
        (Support::Exact, Support::Exact) => Some(Support::Exact),
        _ => Some(Support::Approximated),
    }
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
        Condition::Header { field, test } if field.is_subject() => {
            worst(target, &[RuleFeature::SubjectText(test.op())])
        }
        Condition::Header { test, .. } => worst(target, &[RuleFeature::HeaderText(test.op())]),
        Condition::Address { part, test, .. } => {
            let text = RuleFeature::AddressText(test.op());
            match part {
                AddressPart::All => worst(target, &[text]),
                AddressPart::LocalPart => worst(target, &[text, RuleFeature::AddressLocalPart]),
                AddressPart::Domain => worst(target, &[text, RuleFeature::AddressDomain]),
            }
        }
        Condition::SentToMe(_) => worst(target, &[RuleFeature::SentToMe]),
        Condition::Body(test) => worst(target, &[RuleFeature::Body(test.op())]),
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
                // A role no transport names has no folder to file into.
                RuleMailbox::Role(MailboxRole::Other(_)) => return None,
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
        // Sending on to nobody, or adding no keyword, is no action any transport writes.
        Action::Redirect { to, .. } | Action::Forward { to } if to.is_empty() => None,
        Action::AddKeywords(keywords) if keywords.is_empty() => None,
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
