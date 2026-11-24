//! `plan_rules` on what the source says of a rule itself: a rule the server marks in error, and a
//! redirect that keeps a copy on a target that only redirects that way.

use super::{
    plan_tests::{IMPORT, into, loss, one, rich, subject, without},
    *,
};

fn in_error() -> MailRule {
    MailRule::typed(Match::all(vec![subject("a")]), vec![into("F")]).marked_in_error()
}

#[test]
fn an_import_stores_a_rule_the_source_marks_in_error_disabled() {
    let (written, outcome) = one(&in_error(), &rich(), IMPORT);
    assert_eq!(outcome.kept, Kept::Disabled);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Rule, LossReason::SourceInError)]
    );
    assert!(!written.expect("stored").enabled);
}

#[test]
fn an_import_does_not_keep_a_rule_in_error_where_no_rule_can_be_stored_disabled() {
    let target = without(&rich(), RuleFeature::Disabled);
    let (written, outcome) = one(&in_error(), &target, IMPORT);
    assert_eq!(outcome.kept, Kept::NotKept);
    assert!(written.is_none());
    assert!(
        outcome
            .losses
            .contains(&loss(RulePart::Rule, LossReason::SourceInError))
    );
}

#[test]
fn an_edit_writes_a_rule_in_error_as_given_and_says_so() {
    let (written, outcome) = one(&in_error(), &rich(), PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Rule, LossReason::SourceInError)]
    );
    assert!(written.expect("written").enabled);
}

#[test]
fn a_rule_in_error_the_source_had_disabled_stays_disabled_on_import() {
    let rule = in_error().disabled();
    let (written, outcome) = one(&rule, &rich(), IMPORT);
    assert_eq!(outcome.kept, Kept::Whole);
    assert!(!written.expect("stored").enabled);
}

fn redirect_keeping(copy: bool) -> MailRule {
    MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::Redirect {
            to: vec![RuleAddress::new("r@example.com").unwrap()],
            copy,
        }],
    )
}

#[test]
fn a_redirect_that_keeps_a_copy_needs_only_that_feature() {
    // Graph's `redirectTo` always keeps the message too: its vocabulary lists the copy alone.
    let target =
        without(&rich(), RuleFeature::Redirect).with(RuleFeature::RedirectCopy, Support::Exact);
    let (_, kept) = one(&redirect_keeping(true), &target, PlanMode::Edit);
    assert_eq!(kept.kept, Kept::Whole);
    let (_, dropped) = one(&redirect_keeping(false), &target, PlanMode::Edit);
    assert_eq!(dropped.kept, Kept::NotKept);
    assert_eq!(
        dropped.losses,
        vec![loss(RulePart::Action(0), LossReason::NotExpressible)]
    );
}

#[test]
fn a_forwarding_rule_in_error_is_held_back_either_way_and_not_listed_for_the_switch() {
    // It is stored disabled whatever the forwarding switch says, so the switch decides nothing
    // for it: the preflight list leaves it out, and its outcome says why it is disabled.
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::Redirect {
            to: vec![RuleAddress::new("r@example.com").unwrap()],
            copy: false,
        }],
    )
    .marked_in_error();
    let listed = held_forwarding(std::slice::from_ref(&rule), &rich());
    assert_eq!(listed, Vec::new());
    let (written, outcome) = one(&rule, &rich(), IMPORT);
    assert_eq!(outcome.kept, Kept::Disabled);
    assert!(
        outcome
            .losses
            .contains(&loss(RulePart::Rule, LossReason::SourceInError))
    );
    assert!(!written.expect("stored").enabled);
    // The same rule not in error is the switch's to decide, and listed.
    let mut sound = rule;
    sound.in_error = false;
    assert_eq!(held_forwarding(&[sound], &rich()).len(), 1);
}
