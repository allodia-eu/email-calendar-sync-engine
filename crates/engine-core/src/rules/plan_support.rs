//! How a target holds each part of a rule: the questions `plan_rules` asks of a vocabulary.

use super::{
    Action, AddressField, AddressPart, Condition, HeaderField, Match, MeField, RuleFeature,
    RuleLimit, RuleMailbox, RuleVocabulary, Support, TextOp,
};
use crate::mail::{Keyword, MailboxRole, SystemKeyword};

/// Whether a value of `when`'s conditions or exceptions spans lines.
pub(super) fn spans_lines(when: &Match) -> bool {
    when.conditions.iter().chain(&when.except).any(|condition| {
        let (Condition::Header { test, .. }
        | Condition::Address { test, .. }
        | Condition::Body(test)) = condition
        else {
            return false;
        };
        test.values()
            .iter()
            .any(|value| value.contains(['\r', '\n']))
    })
}

/// Whether `action` sends mail out of the account.
pub(super) fn sends_out(action: &Action) -> bool {
    match action {
        Action::Redirect { .. } | Action::Forward { .. } => true,
        Action::Native(native) => native.sends_mail,
        _ => false,
    }
}

/// The worse of two supports: missing wins, then approximated.
pub(super) fn both(a: Option<Support>, b: Option<Support>) -> Option<Support> {
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

/// The kind a condition is, for [`RuleLimit::OneTestPerKind`]: `None` for one of a provider's own,
/// which carries its own key.
#[derive(PartialEq, Eq)]
pub(super) enum TestKind {
    Header(HeaderField, TextOp),
    Address(AddressField, AddressPart, TextOp),
    SentToMe(MeField),
    Body(TextOp),
    SizeOver,
    SizeUnder,
    HasAttachment,
    Importance,
}

impl TestKind {
    /// Whether the kind holds a single value, which two tests of it cannot share.
    pub(super) fn single_valued(&self) -> bool {
        matches!(self, Self::SizeOver | Self::SizeUnder | Self::Importance)
    }
}

pub(super) fn test_kind(condition: &Condition) -> Option<TestKind> {
    Some(match condition {
        Condition::Header { field, test } => {
            let field = if field.is_subject() {
                HeaderField::Subject
            } else {
                field.clone()
            };
            TestKind::Header(field, test.op())
        }
        Condition::Address { field, part, test } => TestKind::Address(*field, *part, test.op()),
        Condition::SentToMe(field) => TestKind::SentToMe(*field),
        Condition::Body(test) => TestKind::Body(test.op()),
        Condition::Size(super::SizeBound::Over(_)) => TestKind::SizeOver,
        Condition::Size(super::SizeBound::Under(_)) => TestKind::SizeUnder,
        Condition::HasAttachment => TestKind::HasAttachment,
        Condition::Importance(_) => TestKind::Importance,
        Condition::Native(_) => return None,
    })
}

/// The kind an action that names one thing is, for [`RuleLimit::OneActionPerKind`].
#[derive(PartialEq, Eq)]
pub(super) enum ActionKind {
    Move,
    Copy,
    Importance,
}

pub(super) fn action_kind(action: &Action) -> Option<ActionKind> {
    match action {
        Action::FileInto { copy: false, .. } => Some(ActionKind::Move),
        Action::FileInto { copy: true, .. } => Some(ActionKind::Copy),
        Action::MarkImportance(_) => Some(ActionKind::Importance),
        _ => None,
    }
}

/// How `target` holds `condition`.
pub(super) fn condition_support(condition: &Condition, target: &RuleVocabulary) -> Option<Support> {
    match condition {
        Condition::Address {
            field: AddressField::To | AddressField::Cc,
            ..
        } if target.has_limit(RuleLimit::RecipientsTogether) => None,
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
pub(super) fn action_support(action: &Action, target: &RuleVocabulary) -> Option<Support> {
    match action {
        Action::FileInto { mailbox, copy } => {
            let into = match mailbox {
                // The inbox needs no role to be named: every transport has one by name.
                RuleMailbox::Role(MailboxRole::Inbox) | RuleMailbox::Path(_) => {
                    RuleFeature::FileInto
                }
                // A role no transport names has no folder to file into.
                RuleMailbox::Role(MailboxRole::Other(_)) => return None,
                RuleMailbox::Role(
                    MailboxRole::All | MailboxRole::Flagged | MailboxRole::Important,
                ) if target.has_limit(RuleLimit::RealFoldersOnly) => {
                    return None;
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
        // Sending on to nobody, or adding no keyword, is no action any transport writes.
        Action::Redirect { to, .. } | Action::Forward { to } if to.is_empty() => None,
        Action::AddKeywords(keywords) if keywords.is_empty() => None,
        Action::Redirect { copy: false, .. } => worst(target, &[RuleFeature::Redirect]),
        // Sending on while keeping it is a feature of its own: Graph's `redirectTo` always keeps
        // the message, so it holds this and not a redirect that does not.
        Action::Redirect { copy: true, .. } => worst(target, &[RuleFeature::RedirectCopy]),
        Action::Forward { .. } => worst(target, &[RuleFeature::Forward]),
        Action::AddKeywords(keywords)
            if target.has_limit(RuleLimit::NoOtherSystemKeyword)
                && keywords.iter().any(|keyword| {
                    keyword.as_system().is_some_and(|system| {
                        !matches!(system, SystemKeyword::Seen | SystemKeyword::Flagged)
                    })
                }) =>
        {
            None
        }
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
