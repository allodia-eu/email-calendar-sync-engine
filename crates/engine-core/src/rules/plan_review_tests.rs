//! `plan_rules` on what a carried rule, a native action and the redirect limit need, and the
//! corners where a rule would otherwise be kept and not written.

use std::collections::BTreeSet;

use super::{
    plan_tests::{IMPORT, into, loss, one, redirect, rich, subject, without},
    *,
};
use crate::mail::MailboxRole;

/// A text as a reader reads one that requires nothing, names no folder by id and runs no script.
fn sieve_text(text: &str) -> RawRule {
    RawRule::new(RuleSyntax::Sieve, text)
        .requiring(Vec::new())
        .naming_no_folder_by_id()
        .including_no_script()
}

#[test]
fn an_import_does_not_carry_a_rule_carried_whole_that_may_send_mail() {
    let target = rich().with_opaque(RuleSyntax::Sieve, Vec::new());
    let forwarding = MailRule::opaque(sieve_text(
        "if envelope :is \"from\" \"x@example.com\" { redirect \"y@example.net\"; }",
    ));
    let (written, outcome) = one(&forwarding, &target, IMPORT);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Rule, LossReason::ForwardNotAllowed)]
    );
    // Armed on request, or the user's own edit: carried.
    let enabled = PlanMode::Import {
        forwarding: ForwardingRules::Enabled,
    };
    assert_eq!(one(&forwarding, &target, enabled).1.kept, Kept::Whole);
    assert_eq!(
        one(&forwarding, &target, PlanMode::Edit).1.kept,
        Kept::Whole
    );
    // Text the reader found sends nothing is carried by an import too.
    let quiet = MailRule::opaque(sieve_text("keep;").sending_no_mail());
    assert_eq!(one(&quiet, &target, IMPORT).1.kept, Kept::Whole);
}

#[test]
fn an_import_disables_a_rule_whose_native_action_may_send_mail() {
    let graph = rich().with_native(NativeKind::Graph);
    let forwards = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::Native(NativeAction::new(NativeKind::Graph, "{}"))],
    );
    let (written, outcome) = one(&forwards, &graph, IMPORT);
    assert_eq!(outcome.kept, Kept::Disabled);
    assert_eq!(written.map(|rule| rule.enabled), Some(false));
    let quiet = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::Native(
            NativeAction::new(NativeKind::Graph, "{}").sending_no_mail(),
        )],
    );
    assert_eq!(one(&quiet, &graph, IMPORT).1.kept, Kept::Whole);
}

#[test]
fn a_rule_carried_whole_keeps_its_own_name_and_cannot_be_disabled() {
    let target = rich().with_opaque(RuleSyntax::Sieve, Vec::new());
    let raw = sieve_text("keep;")
        .sending_no_mail()
        .named(RuleName::new("As read").unwrap());
    let renamed = MailRule::opaque(raw.clone()).named(RuleName::new("New").unwrap());
    let (written, outcome) = one(&renamed, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Name, LossReason::NotExpressible)]
    );
    assert_eq!(
        written.unwrap().name.unwrap().as_str(),
        "As read",
        "written with the name its text holds"
    );
    let disabled = MailRule::opaque(raw.clone())
        .named(RuleName::new("As read").unwrap())
        .disabled();
    let (written, outcome) = one(&disabled, &target, PlanMode::Edit);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Enabled, LossReason::NotExpressible)]
    );
}

#[test]
fn a_rule_carried_whole_needs_every_extension_it_requires_listed() {
    let raw = RawRule::new(RuleSyntax::Sieve, "x;")
        .requiring(["vacation".to_owned()])
        .sending_no_mail();
    let rule = MailRule::opaque(raw);
    let without = rich().with_opaque(RuleSyntax::Sieve, ["fileinto".to_owned()]);
    assert_eq!(one(&rule, &without, PlanMode::Edit).1.kept, Kept::NotKept);
    let with = rich().with_opaque(RuleSyntax::Sieve, ["vacation".to_owned()]);
    assert_eq!(one(&rule, &with, PlanMode::Edit).1.kept, Kept::Whole);
}

#[test]
fn what_no_transport_writes_is_not_expressible() {
    let target = rich()
        .with_exact([RuleFeature::FileIntoRole, RuleFeature::Flag])
        .with_max_redirects(5);
    for (what, action) in [
        (
            "a role no transport names",
            Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Other("x".into())),
                copy: false,
            },
        ),
        ("no keyword", Action::AddKeywords(BTreeSet::new())),
        ("a redirect to nobody", redirect(0)),
        ("a forward to nobody", Action::Forward { to: Vec::new() }),
    ] {
        let rule = MailRule::typed(Match::default(), vec![action]);
        let (written, outcome) = one(&rule, &target, PlanMode::Edit);
        assert_eq!(written, None, "{what}");
        assert_eq!(
            outcome.losses,
            vec![loss(RulePart::Action(0), LossReason::NotExpressible)],
            "{what}"
        );
    }
}

#[test]
fn the_redirect_limit_counts_every_address_of_the_rule() {
    let target = rich().with_max_redirects(2);
    let rule = MailRule::typed(
        Match::default(),
        vec![redirect(1), redirect(1), redirect(1), into("x")],
    );
    let (written, outcome) = one(&rule, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Action(2), LossReason::ForwardNotAllowed)]
    );
    assert_eq!(
        written,
        Some(MailRule::typed(
            Match::default(),
            vec![redirect(1), redirect(1), into("x")]
        ))
    );
}

#[test]
fn a_rule_whose_only_effect_was_its_lost_stop_is_not_kept() {
    let rule = MailRule::typed(Match::all(vec![subject("a")]), Vec::new()).stopping();
    let (written, outcome) = one(&rule, &without(&rich(), RuleFeature::Stop), PlanMode::Edit);
    assert_eq!(written, None);
    assert_eq!(outcome.kept, Kept::NotKept);
    // With its stop held, it is kept as it is.
    assert_eq!(one(&rule, &rich(), PlanMode::Edit).1.kept, Kept::Whole);
}

#[test]
fn an_exception_held_approximately_says_so() {
    let target = rich().with(RuleFeature::Except, Support::Approximated);
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]).except(vec![subject("b")]),
        vec![into("x")],
    );
    let (_, outcome) = one(&rule, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Exception(0), LossReason::Approximated)]
    );
}

#[test]
fn a_disabled_import_still_lists_what_else_it_lost() {
    let target = without(&rich(), RuleFeature::Name);
    let rule = MailRule::typed(Match::all(vec![subject("a")]), vec![redirect(1)])
        .named(RuleName::new("n").unwrap());
    let (_, outcome) = one(&rule, &target, IMPORT);
    assert_eq!(outcome.kept, Kept::Disabled);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Name, LossReason::NotExpressible)]
    );
}

#[test]
fn a_subject_named_in_any_case_is_the_subject() {
    let named = Condition::Header {
        field: HeaderField::Named("SUBJECT".to_owned()),
        test: TextTest::new(TextOp::Contains, vec!["a".to_owned()]).unwrap(),
    };
    let rule = MailRule::typed(Match::all(vec![named]), vec![into("x")]);
    assert_eq!(one(&rule, &rich(), PlanMode::Edit).1.kept, Kept::Whole);
    assert_eq!(HeaderField::named("Subject"), HeaderField::Subject);
    assert_eq!(
        HeaderField::named("X-Spam"),
        HeaderField::Named("X-Spam".to_owned())
    );
}

#[test]
fn a_carried_rule_whose_strings_hold_an_expansion_does_not_move_to_another_script() {
    let target = rich().with_opaque(RuleSyntax::Sieve, Vec::new());
    for expansion in [Expansion::Literal, Expansion::Expanded] {
        let rule = MailRule::opaque(sieve_text("x;").sending_no_mail().holding(expansion));
        let (written, outcome) = one(&rule, &target, IMPORT);
        assert_eq!(written, None, "{expansion:?}");
        assert_eq!(
            outcome.losses,
            vec![loss(RulePart::Rule, LossReason::NotExpressible)]
        );
        // Kept where it is: the writer checks the script it is written over.
        assert_eq!(one(&rule, &target, PlanMode::Edit).1.kept, Kept::Whole);
    }
}

#[test]
fn a_disabled_rule_whose_test_spans_lines_needs_a_target_that_stores_one() {
    let body = subject("a\r\nb");
    let rule = MailRule::typed(Match::all(vec![body.clone()]), vec![into("x")]).disabled();
    let (written, outcome) = one(&rule, &rich(), PlanMode::Edit);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Enabled, LossReason::NotExpressible)]
    );
    let holds = rich().with_exact([RuleFeature::DisabledMultiLine]);
    assert_eq!(one(&rule, &holds, PlanMode::Edit).1.kept, Kept::Whole);
    // Enabled, it needs nothing more; and an import it would disable for review is refused
    // for the same reason, as a forwarding rule the target cannot hold disabled.
    let enabled = MailRule::typed(Match::all(vec![body.clone()]), vec![into("x")]);
    assert_eq!(one(&enabled, &rich(), PlanMode::Edit).1.kept, Kept::Whole);
    let forwards = MailRule::typed(Match::all(vec![body]), vec![redirect(1)]);
    let (written, outcome) = one(&forwards, &rich(), IMPORT);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Enabled, LossReason::ForwardNotAllowed)]
    );
}
