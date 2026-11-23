//! `plan_rules` against hand-built vocabularies: the two safety rules, each reason, and how
//! a reduced rule is written. The per-transport tables (section 5 of the design note) are where
//! each vocabulary is built: `engine-sieve`'s for Sieve.

use std::collections::BTreeSet;

use super::*;
use crate::mail::{Importance, Keyword, MailboxRole, SystemKeyword};

pub(super) const IMPORT: PlanMode = PlanMode::Import {
    forwarding: ForwardingRules::Disabled,
};

pub(super) fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap(),
    }
}

pub(super) fn into(path: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Path(MailboxPath::new(path).unwrap()),
        copy: false,
    }
}

pub(super) fn redirect(count: usize) -> Action {
    Action::Redirect {
        to: (0..count)
            .map(|i| RuleAddress::new(format!("r{i}@example.com")).unwrap())
            .collect(),
        copy: false,
    }
}

/// A language with everything the tests below need, exactly.
pub(super) fn rich() -> RuleVocabulary {
    RuleVocabulary::new().with_exact([
        RuleFeature::MatchAny,
        RuleFeature::Except,
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::SizeOver,
        RuleFeature::FileInto,
        RuleFeature::Discard,
        RuleFeature::Redirect,
        RuleFeature::MarkRead,
        RuleFeature::Stop,
        RuleFeature::Disabled,
        RuleFeature::Name,
        RuleFeature::Order,
    ])
}

pub(super) fn without(vocabulary: &RuleVocabulary, missing: RuleFeature) -> RuleVocabulary {
    let mut rebuilt = RuleVocabulary::new();
    for feature in ALL {
        if feature != missing
            && let Some(support) = vocabulary.support(feature)
        {
            rebuilt = rebuilt.with(feature, support);
        }
    }
    rebuilt
}

const ALL: [RuleFeature; 12] = [
    RuleFeature::MatchAny,
    RuleFeature::Except,
    RuleFeature::SubjectText(TextOp::Contains),
    RuleFeature::SizeOver,
    RuleFeature::FileInto,
    RuleFeature::Discard,
    RuleFeature::Redirect,
    RuleFeature::MarkRead,
    RuleFeature::Stop,
    RuleFeature::Disabled,
    RuleFeature::Name,
    RuleFeature::Order,
];

pub(super) fn one(
    rule: &MailRule,
    target: &RuleVocabulary,
    mode: PlanMode,
) -> (Option<MailRule>, RuleOutcome) {
    let plan = plan_rules(std::slice::from_ref(rule), target, mode);
    assert_eq!(plan.outcomes.len(), 1);
    (
        plan.rules.into_iter().next().map(|planned| planned.rule),
        plan.outcomes[0].clone(),
    )
}

pub(super) fn loss(part: RulePart, reason: LossReason) -> Loss {
    Loss { part, reason }
}

#[test]
fn a_rule_the_target_holds_is_kept_whole_and_written_as_given() {
    let rule = MailRule::typed(Match::all(vec![subject("Invoice")]), vec![into("Finance")])
        .named(RuleName::new("Invoices").unwrap())
        .stopping();
    let (written, outcome) = one(&rule, &rich(), PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(outcome.losses, Vec::new());
    assert_eq!(written, Some(rule));
}

#[test]
fn a_lost_exception_drops_the_whole_rule() {
    // "Delete newsletters, except from my bank": without the exception it deletes the bank's.
    let rule = MailRule::typed(
        Match::all(vec![subject("newsletter")]).except(vec![subject("bank")]),
        vec![Action::Discard],
    );
    let (written, outcome) = one(
        &rule,
        &without(&rich(), RuleFeature::Except),
        PlanMode::Edit,
    );
    assert_eq!(written, None);
    assert_eq!(outcome.kept, Kept::NotKept);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Exception(0), LossReason::NotExpressible)]
    );
    // An exception the target holds as such but whose test it cannot express: the same.
    let rule = MailRule::typed(
        Match::all(vec![subject("newsletter")]).except(vec![Condition::HasAttachment]),
        vec![Action::Discard],
    );
    assert_eq!(one(&rule, &rich(), PlanMode::Edit).1.kept, Kept::NotKept);
}

#[test]
fn a_lost_condition_or_way_of_combining_drops_the_whole_rule() {
    let rule = MailRule::typed(
        Match::all(vec![subject("a"), Condition::Importance(Importance::High)]),
        vec![into("x")],
    );
    let (written, outcome) = one(&rule, &rich(), PlanMode::Edit);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Condition(1), LossReason::NotExpressible)]
    );

    let any = MailRule::typed(
        Match::any(vec![subject("a"), subject("b")]),
        vec![into("x")],
    );
    let (written, outcome) = one(
        &any,
        &without(&rich(), RuleFeature::MatchAny),
        PlanMode::Edit,
    );
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Mode, LossReason::NotExpressible)]
    );
    // One condition under "any" is the same as under "all".
    let single = MailRule::typed(Match::any(vec![subject("a")]), vec![into("x")]);
    let (_, outcome) = one(
        &single,
        &without(&rich(), RuleFeature::MatchAny),
        PlanMode::Edit,
    );
    assert_eq!(outcome.kept, Kept::Whole);
}

#[test]
fn a_lost_action_or_stop_or_name_reduces_the_rule_and_is_not_written() {
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![
            into("x"),
            Action::MarkImportance(Importance::High),
            redirect(1),
        ],
    )
    .named(RuleName::new("n").unwrap())
    .stopping();
    let target = without(&without(&rich(), RuleFeature::Stop), RuleFeature::Name);
    let (written, outcome) = one(&rule, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![
            loss(RulePart::Action(1), LossReason::NotExpressible),
            loss(RulePart::Stop, LossReason::NotExpressible),
            loss(RulePart::Name, LossReason::NotExpressible),
        ]
    );
    let expected = MailRule::typed(Match::all(vec![subject("a")]), vec![into("x"), redirect(1)]);
    assert_eq!(written, Some(expected));
}

#[test]
fn a_rule_left_with_no_action_is_not_kept() {
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::MarkImportance(Importance::Low)],
    );
    let (written, outcome) = one(&rule, &rich(), PlanMode::Edit);
    assert_eq!((written, outcome.kept), (None, Kept::NotKept));
}

#[test]
fn an_approximated_part_keeps_the_rule_whole_and_says_so() {
    let target = rich().with(RuleFeature::Forward, Support::Approximated);
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::Forward {
            to: vec![RuleAddress::new("f@example.com").unwrap()],
        }],
    );
    let (written, outcome) = one(&rule, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Action(0), LossReason::Approximated)]
    );
    assert_eq!(written, Some(rule));
    // An approximated condition, and the worst of several features (copy approximated).
    let target = rich()
        .with(RuleFeature::FileIntoCopy, Support::Approximated)
        .with(RuleFeature::SizeOver, Support::Approximated);
    let rule = MailRule::typed(
        Match::all(vec![Condition::Size(SizeBound::Over(10))]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Path(MailboxPath::new("x").unwrap()),
            copy: true,
        }],
    );
    let (_, outcome) = one(&rule, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(outcome.losses.len(), 2);
}

#[test]
fn an_import_stores_a_forwarding_rule_disabled_unless_told_otherwise() {
    let rule = MailRule::typed(Match::all(vec![subject("a")]), vec![redirect(1)]);
    let (written, outcome) = one(&rule, &rich(), IMPORT);
    assert_eq!(outcome.kept, Kept::Disabled);
    assert_eq!(written.map(|rule| rule.enabled), Some(false));

    let enabled = PlanMode::Import {
        forwarding: ForwardingRules::Enabled,
    };
    let (written, outcome) = one(&rule, &rich(), enabled);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(written.map(|rule| rule.enabled), Some(true));

    // The user's own edit is written as given.
    assert_eq!(one(&rule, &rich(), PlanMode::Edit).1.kept, Kept::Whole);
    // A target that cannot store a disabled rule does not get it at all.
    let (written, outcome) = one(&rule, &without(&rich(), RuleFeature::Disabled), IMPORT);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Enabled, LossReason::ForwardNotAllowed)]
    );
    // A rule that forwards nothing is imported as it ran.
    let plain = MailRule::typed(Match::all(vec![subject("a")]), vec![into("x")]);
    assert_eq!(one(&plain, &rich(), IMPORT).1.kept, Kept::Whole);
}

#[test]
fn a_disabled_rule_needs_a_target_that_stores_one() {
    let rule = MailRule::typed(Match::all(vec![subject("a")]), vec![into("x")]).disabled();
    assert_eq!(one(&rule, &rich(), PlanMode::Edit).1.kept, Kept::Whole);
    let (written, outcome) = one(
        &rule,
        &without(&rich(), RuleFeature::Disabled),
        PlanMode::Edit,
    );
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Enabled, LossReason::NotExpressible)]
    );
}

#[test]
fn more_addresses_than_the_target_sends_to_lose_the_action() {
    let target = rich().with_max_redirects(1);
    let rule = MailRule::typed(Match::default(), vec![redirect(2), into("x")]);
    let (written, outcome) = one(&rule, &target, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Action(0), LossReason::ForwardNotAllowed)]
    );
    assert_eq!(
        written,
        Some(MailRule::typed(Match::default(), vec![into("x")]))
    );
    assert_eq!(
        one(
            &MailRule::typed(Match::default(), vec![redirect(1)]),
            &target,
            PlanMode::Edit
        )
        .1
        .kept,
        Kept::Whole
    );
}

#[test]
fn an_opaque_rule_goes_only_to_its_own_kind() {
    let rule = MailRule::opaque(RawRule::new(RuleSyntax::Sieve, "keep;"));
    let (written, outcome) = one(&rule, &rich(), PlanMode::Edit);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Rule, LossReason::OpaqueSource)]
    );
    let rule = MailRule::opaque(
        RawRule::new(RuleSyntax::Sieve, "keep;")
            .requiring(Vec::new())
            .sending_no_mail(),
    );
    let sieve = rich().with_opaque(RuleSyntax::Sieve, Vec::new());
    assert_eq!(one(&rule, &sieve, PlanMode::Edit).0, Some(rule));
}

#[test]
fn a_native_part_goes_only_to_its_own_kind() {
    let rule = MailRule::typed(
        Match::all(vec![Condition::Native(NativeCondition::new(
            NativeKind::Graph,
            "{}",
        ))]),
        vec![Action::Native(NativeAction::new(NativeKind::Graph, "{}"))],
    );
    assert_eq!(one(&rule, &rich(), PlanMode::Edit).1.kept, Kept::NotKept);
    let graph = rich().with_native(NativeKind::Graph);
    assert_eq!(one(&rule, &graph, PlanMode::Edit).1.kept, Kept::Whole);
}

#[test]
fn order_is_lost_only_where_there_is_more_than_one_rule() {
    let rule = MailRule::typed(Match::default(), vec![into("x")]);
    let target = without(&rich(), RuleFeature::Order);
    assert_eq!(one(&rule, &target, PlanMode::Edit).1.kept, Kept::Whole);
    let plan = plan_rules(&[rule.clone(), rule], &target, PlanMode::Edit);
    assert_eq!(plan.rules.len(), 2);
    assert!(
        plan.outcomes
            .iter()
            .all(|outcome| outcome.kept == Kept::Reduced
                && outcome.losses == vec![loss(RulePart::Order, LossReason::NotExpressible)])
    );
}

#[test]
fn keywords_need_the_feature_their_kind_needs() {
    let seen: BTreeSet<Keyword> = [Keyword::system(SystemKeyword::Seen)].into();
    let flagged: BTreeSet<Keyword> = [Keyword::system(SystemKeyword::Flagged)].into();
    let named: BTreeSet<Keyword> = [Keyword::new("work").unwrap()].into();
    let target = rich();
    let kept = |keywords: BTreeSet<Keyword>| {
        let rule = MailRule::typed(Match::default(), vec![Action::AddKeywords(keywords)]);
        one(&rule, &target, PlanMode::Edit).1.kept
    };
    assert_eq!(kept(seen), Kept::Whole);
    assert_eq!(kept(flagged), Kept::NotKept);
    assert_eq!(kept(named), Kept::NotKept);
}

#[test]
fn every_condition_and_action_asks_for_its_own_feature() {
    let everything = RuleVocabulary::new().with_exact([
        RuleFeature::HeaderText(TextOp::Regex),
        RuleFeature::AddressText(TextOp::Is),
        RuleFeature::AddressLocalPart,
        RuleFeature::AddressDomain,
        RuleFeature::SentToMe,
        RuleFeature::Body(TextOp::Matches),
        RuleFeature::SizeUnder,
        RuleFeature::HasAttachment,
        RuleFeature::Importance,
        RuleFeature::FileIntoRole,
        RuleFeature::FileIntoId,
        RuleFeature::Discard,
        RuleFeature::Redirect,
        RuleFeature::RedirectCopy,
        RuleFeature::Forward,
        RuleFeature::Flag,
        RuleFeature::NamedKeyword,
        RuleFeature::MarkImportance,
    ]);
    let test = |op| TextTest::new(op, vec!["v".to_owned()]).unwrap();
    let address = |part| Condition::Address {
        field: AddressField::ToOrCc,
        part,
        test: test(TextOp::Is),
    };
    let rule = MailRule::typed(
        Match::all(vec![
            Condition::Header {
                field: HeaderField::Named("X-Spam".to_owned()),
                test: test(TextOp::Regex),
            },
            address(AddressPart::All),
            address(AddressPart::LocalPart),
            address(AddressPart::Domain),
            Condition::SentToMe(MeField::To),
            Condition::Body(test(TextOp::Matches)),
            Condition::Size(SizeBound::Under(5)),
            Condition::HasAttachment,
            Condition::Importance(Importance::Low),
        ]),
        vec![
            Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Junk),
                copy: false,
            },
            Action::FileInto {
                mailbox: RuleMailbox::Id(crate::ids::MailboxId::try_from("m").unwrap()),
                copy: false,
            },
            Action::Discard,
            Action::Redirect {
                to: vec![RuleAddress::new("a@example.com").unwrap()],
                copy: true,
            },
            Action::Forward {
                to: vec![RuleAddress::new("a@example.com").unwrap()],
            },
            Action::AddKeywords(
                [
                    Keyword::system(SystemKeyword::Flagged),
                    Keyword::new("work").unwrap(),
                ]
                .into(),
            ),
            Action::MarkImportance(Importance::High),
        ],
    );
    let (written, outcome) = one(&rule, &everything, PlanMode::Edit);
    assert_eq!(outcome.kept, Kept::Whole, "{:?}", outcome.losses);
    assert_eq!(written, Some(rule.clone()));
    // Each one, taken away, is what the plan names.
    let RuleBody::Typed(typed) = &rule.body else {
        unreachable!()
    };
    assert_eq!(typed.when.conditions.len(), 9);
    assert_eq!(
        one(&rule, &RuleVocabulary::new(), PlanMode::Edit)
            .1
            .losses
            .len(),
        9 + 7
    );
}
