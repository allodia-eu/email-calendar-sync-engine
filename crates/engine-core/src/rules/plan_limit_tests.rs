//! `plan_rules` against a target's limits on how parts combine ([`RuleLimit`]): what a target
//! holds part by part and still cannot hold together (Graph's predicates, keys of one object).

use std::collections::BTreeSet;

use super::{
    plan_tests::{loss, one, rich, subject},
    *,
};
use crate::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
};

fn limited(limit: RuleLimit) -> RuleVocabulary {
    rich()
        .with_exact([
            RuleFeature::AddressText(TextOp::Contains),
            RuleFeature::FileIntoId,
            RuleFeature::MarkImportance,
            RuleFeature::NamedKeyword,
            RuleFeature::Importance,
        ])
        .with_limit(limit)
}

fn to(field: AddressField) -> Condition {
    Condition::Address {
        field,
        part: AddressPart::All,
        test: TextTest::new(TextOp::Contains, vec!["x".to_owned()]).unwrap(),
    }
}

fn move_to(id: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(id).unwrap()),
        copy: false,
    }
}

const EDIT: PlanMode = PlanMode::Edit;

#[test]
fn a_target_without_limits_plans_as_before() {
    let target = limited(RuleLimit::RecipientsTogether);
    assert!(target.has_limit(RuleLimit::RecipientsTogether));
    assert!(!rich().has_limit(RuleLimit::RecipientsTogether));
    let unlimited = rich().with_exact([RuleFeature::AddressText(TextOp::Contains)]);
    let rule = MailRule::typed(
        Match::all(vec![subject("a"), subject("b"), to(AddressField::To)]),
        vec![Action::Discard],
    );
    assert_eq!(one(&rule, &unlimited, EDIT).1.kept, Kept::Whole);
}

#[test]
fn to_or_cc_alone_is_not_held_where_recipients_are_tested_together() {
    let target = limited(RuleLimit::RecipientsTogether);
    for field in [AddressField::To, AddressField::Cc] {
        let rule = MailRule::typed(Match::all(vec![to(field)]), vec![Action::Discard]);
        let (written, outcome) = one(&rule, &target, EDIT);
        assert!(written.is_none());
        assert_eq!(
            outcome.losses,
            vec![loss(RulePart::Condition(0), LossReason::NotExpressible)]
        );
    }
    for field in [AddressField::ToOrCc, AddressField::From] {
        let rule = MailRule::typed(Match::all(vec![to(field)]), vec![Action::Discard]);
        assert_eq!(one(&rule, &target, EDIT).1.kept, Kept::Whole);
    }
}

#[test]
fn a_second_test_of_one_kind_is_not_held_where_each_kind_is_one_test() {
    let target = limited(RuleLimit::OneTestPerKind);
    let rule = MailRule::typed(
        Match::all(vec![subject("a"), to(AddressField::From), subject("b")]),
        vec![Action::Discard],
    );
    let (written, outcome) = one(&rule, &target, EDIT);
    assert!(written.is_none());
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Condition(2), LossReason::NotExpressible)]
    );
    // Among exceptions, where any one vetoes, two tests of a kind with values are one any-of
    // test: a Roundcube rule with two sender exceptions carries whole.
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]).except(vec![to(AddressField::From), to(AddressField::From)]),
        vec![Action::Discard],
    );
    assert_eq!(one(&rule, &target, EDIT).1.kept, Kept::Whole);
    // A kind with one value cannot hold two: two importances are not one.
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]).except(vec![
            Condition::Importance(Importance::High),
            Condition::Importance(Importance::Low),
        ]),
        vec![Action::Discard],
    );
    let (_, outcome) = one(&rule, &target, EDIT);
    assert_eq!(outcome.kept, Kept::NotKept);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Exception(1), LossReason::NotExpressible)]
    );
    // One test with two values is one test.
    let two_values = Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec!["a".into(), "b".into()]).unwrap(),
    };
    let rule = MailRule::typed(Match::all(vec![two_values]), vec![Action::Discard]);
    assert_eq!(one(&rule, &target, EDIT).1.kept, Kept::Whole);
}

#[test]
fn a_second_action_of_one_kind_is_lost_and_the_rule_reduced() {
    let target = limited(RuleLimit::OneActionPerKind);
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![
            move_to("f1"),
            Action::MarkImportance(Importance::High),
            move_to("f2"),
            Action::MarkImportance(Importance::Low),
            Action::Discard,
            Action::Discard,
        ],
    );
    let (written, outcome) = one(&rule, &target, EDIT);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![
            loss(RulePart::Action(2), LossReason::NotExpressible),
            loss(RulePart::Action(3), LossReason::NotExpressible),
        ]
    );
    let RuleBody::Typed(written) = written.unwrap().body else {
        panic!("typed")
    };
    assert_eq!(written.actions.len(), 4);
}

#[test]
fn a_system_keyword_but_seen_and_flagged_is_lost_where_the_target_keeps_none() {
    let target = limited(RuleLimit::NoOtherSystemKeyword);
    let answered = Action::AddKeywords(BTreeSet::from([Keyword::system(SystemKeyword::Answered)]));
    let named = Action::AddKeywords(BTreeSet::from([Keyword::new("project").unwrap()]));
    let rule = MailRule::typed(Match::all(vec![subject("a")]), vec![answered, named]);
    let (_, outcome) = one(&rule, &target, EDIT);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Action(0), LossReason::NotExpressible)]
    );
}

#[test]
fn a_virtual_collection_is_no_folder_to_file_into_where_only_folders_are() {
    let target = rich()
        .with_exact([RuleFeature::FileIntoRole])
        .with_limit(RuleLimit::RealFoldersOnly);
    let file = |role: MailboxRole| {
        MailRule::typed(
            Match::all(vec![subject("a")]),
            vec![Action::FileInto {
                mailbox: RuleMailbox::Role(role),
                copy: false,
            }],
        )
    };
    for role in [
        MailboxRole::All,
        MailboxRole::Flagged,
        MailboxRole::Important,
    ] {
        assert_eq!(one(&file(role), &target, EDIT).1.kept, Kept::NotKept);
    }
    assert_eq!(
        one(&file(MailboxRole::Junk), &target, EDIT).1.kept,
        Kept::Whole
    );
    let unlimited = rich().with_exact([RuleFeature::FileIntoRole]);
    assert_eq!(
        one(&file(MailboxRole::All), &unlimited, EDIT).1.kept,
        Kept::Whole
    );
}
