//! `plan_rules` against a target that matches text as a search does (Gmail's filters): a word
//! and phrase match of its own, exceptions that must be held exactly, and the limits a filter has
//! that its vocabulary's features alone cannot say.

use super::{
    plan_tests::{loss, one, subject},
    *,
};
use crate::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole},
};

const EDIT: PlanMode = PlanMode::Edit;

fn words(value: &str) -> TextTest {
    TextTest::new(TextOp::Words, vec![value.to_owned()]).unwrap()
}

fn sender(part: AddressPart, test: TextTest) -> Condition {
    Condition::Address {
        field: AddressField::From,
        part,
        test,
    }
}

/// A search target: word matches exact, a subject that contains approximated.
fn search() -> RuleVocabulary {
    RuleVocabulary::new()
        .with_exact([
            RuleFeature::Except,
            RuleFeature::SubjectText(TextOp::Words),
            RuleFeature::AddressText(TextOp::Words),
            RuleFeature::AddressWithName,
            RuleFeature::Discard,
            RuleFeature::FileIntoId,
            RuleFeature::FileIntoCopy,
            RuleFeature::FileIntoRole,
            RuleFeature::MarkImportance,
            RuleFeature::NamedKeyword,
        ])
        .with(
            RuleFeature::SubjectText(TextOp::Contains),
            Support::Approximated,
        )
        .with_native(NativeKind::Gmail)
}

fn discard(when: Match) -> MailRule {
    MailRule::typed(when, vec![Action::Discard])
}

/// A word match is a test of its own: a target that does not list it loses the condition, and
/// with it the rule, as every transport but a search does.
#[test]
fn a_word_match_is_held_only_where_listed() {
    let rule = discard(Match::all(vec![
        Condition::Header {
            field: HeaderField::Subject,
            test: words("invoice"),
        },
        sender(AddressPart::WithName, words("alice")),
    ]));
    assert_eq!(one(&rule, &search(), EDIT).1.kept, Kept::Whole);
    let contains_only = RuleVocabulary::new().with_exact([
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::AddressText(TextOp::Contains),
        RuleFeature::Discard,
    ]);
    let (written, outcome) = one(&rule, &contains_only, EDIT);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        [
            loss(RulePart::Condition(0), LossReason::NotExpressible),
            loss(RulePart::Condition(1), LossReason::NotExpressible),
        ]
    );
    // The address with its display name needs the target to test the name too.
    let no_name = search().without(RuleFeature::AddressWithName);
    assert_eq!(
        one(&rule, &no_name, EDIT).1.losses,
        [loss(RulePart::Condition(1), LossReason::NotExpressible)]
    );
}

/// An approximation narrows what a test matches: as a condition the rule does less, and as an
/// exception it vetoes less, so the rule does more. Where the target says so, it is lost there.
#[test]
fn an_approximated_exception_is_lost_where_exceptions_must_be_exact() {
    let target = search().with_limit(RuleLimit::ExactExceptions);
    let condition = discard(Match::all(vec![subject("a")]));
    let (_, outcome) = one(&condition, &target, EDIT);
    assert_eq!(outcome.kept, Kept::Whole);
    assert_eq!(
        outcome.losses,
        [loss(RulePart::Condition(0), LossReason::Approximated)]
    );
    let exception = discard(
        Match::all(vec![sender(AddressPart::WithName, words("a"))]).except(vec![subject("b")]),
    );
    let (written, outcome) = one(&exception, &target, EDIT);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        [loss(RulePart::Exception(0), LossReason::NotExpressible)]
    );
    assert_eq!(one(&exception, &search(), EDIT).1.kept, Kept::Whole);
}

#[test]
fn an_importance_marked_is_high_or_low_where_the_target_has_no_normal() {
    let target = search().with_limit(RuleLimit::ImportanceHighOrLow);
    let mark = |importance| {
        MailRule::typed(
            Match::all(vec![subject("a")]),
            vec![Action::MarkImportance(importance), Action::Discard],
        )
    };
    assert_eq!(
        one(&mark(Importance::High), &target, EDIT).1.losses.len(),
        1
    );
    assert_eq!(one(&mark(Importance::Low), &target, EDIT).1.losses.len(), 1);
    assert_eq!(
        one(&mark(Importance::Normal), &target, EDIT).1.losses[1],
        loss(RulePart::Action(0), LossReason::NotExpressible)
    );
}

/// A target that keeps a keyword only under a name it was given holds those keywords alone.
#[test]
fn a_named_keyword_is_held_only_where_the_target_has_its_name() {
    let work = Keyword::new("$work").unwrap();
    let other = Keyword::new("$other").unwrap();
    let target = search().with_keywords([work.clone()]);
    let add = |keyword: &Keyword| {
        MailRule::typed(
            Match::all(vec![sender(AddressPart::WithName, words("a"))]),
            vec![
                Action::AddKeywords([keyword.clone()].into()),
                Action::Discard,
            ],
        )
    };
    assert_eq!(one(&add(&work), &target, EDIT).1.kept, Kept::Whole);
    assert_eq!(
        one(&add(&other), &target, EDIT).1.losses,
        [loss(RulePart::Action(0), LossReason::NotExpressible)]
    );
    assert_eq!(
        target.keywords().map(std::collections::BTreeSet::len),
        Some(1)
    );
    assert_eq!(search().keywords(), None);
}

#[test]
fn a_native_condition_holds_only_among_conditions_that_must_all_hold() {
    let target = search()
        .with(RuleFeature::MatchAny, Support::Exact)
        .with_limit(RuleLimit::NativeConditionsAllOf);
    let native = Condition::Native(NativeCondition::new(NativeKind::Gmail, "{}"));
    let all = discard(Match::all(vec![native.clone(), subject("a")]));
    assert_eq!(one(&all, &target, EDIT).1.losses.len(), 1); // the approximated subject
    let any = discard(Match::any(vec![native.clone(), subject("a")]));
    assert_eq!(
        one(&any, &target, EDIT).1.losses[0],
        loss(RulePart::Condition(0), LossReason::NotExpressible)
    );
    let except = discard(Match::all(vec![subject("a")]).except(vec![native]));
    assert!(
        one(&except, &target, EDIT)
            .1
            .losses
            .contains(&loss(RulePart::Exception(0), LossReason::NotExpressible))
    );
}

#[test]
fn a_value_is_held_only_as_a_phrase_where_the_target_searches() {
    let target = search().with_limit(RuleLimit::PhraseValues);
    for (value, held) in [
        ("weekly report", true),
        ("x+tag@example.com", true),
        ("OR", true),
        ("say \"hi\"", false),
        ("two\nlines", false),
        ("   ", false),
        ("", false),
        ("me", false),
        ("Me", false),
    ] {
        assert_eq!(is_phrase(value), held, "{value:?}");
        let Ok(test) = TextTest::new(TextOp::Words, vec!["fine".to_owned(), value.to_owned()])
        else {
            continue;
        };
        let rule = discard(Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test,
        }]));
        let kept = one(&rule, &target, EDIT).1.kept;
        assert_eq!(kept == Kept::Whole, held, "{value:?}");
    }
}

#[test]
fn only_trash_junk_and_the_archive_are_filed_into_where_roles_are_labels() {
    let target = search().with_limit(RuleLimit::TrashJunkArchiveOnly);
    let into = |role: MailboxRole, copy| {
        MailRule::typed(
            Match::all(vec![sender(AddressPart::WithName, words("a"))]),
            vec![
                Action::FileInto {
                    mailbox: RuleMailbox::Role(role),
                    copy,
                },
                Action::Discard,
            ],
        )
    };
    for role in [MailboxRole::Trash, MailboxRole::Junk, MailboxRole::Archive] {
        assert_eq!(
            one(&into(role.clone(), false), &target, EDIT).1.kept,
            Kept::Whole
        );
        assert_eq!(one(&into(role, true), &target, EDIT).1.kept, Kept::Reduced);
    }
    for role in [MailboxRole::Sent, MailboxRole::Drafts] {
        assert_eq!(one(&into(role, false), &target, EDIT).1.kept, Kept::Reduced);
    }
    // A folder by id is no role.
    let folder = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Id(MailboxId::try_from("Label_1").unwrap()),
            copy: true,
        }],
    );
    assert_ne!(one(&folder, &target, EDIT).1.kept, Kept::NotKept);
}

/// A provider's own action that sends mail counts against the redirect limit as one address, so
/// a typed forward and a native one are never both written where only one is held.
#[test]
fn a_native_action_that_sends_mail_counts_as_one_address() {
    let target = search()
        .with(RuleFeature::RedirectCopy, Support::Exact)
        .with_max_redirects(1);
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![
            Action::Native(NativeAction::new(NativeKind::Gmail, "{}")),
            Action::Redirect {
                to: vec![RuleAddress::new("a@example.com").unwrap()],
                copy: true,
            },
        ],
    );
    assert!(
        one(&rule, &target, EDIT)
            .1
            .losses
            .contains(&loss(RulePart::Action(1), LossReason::ForwardNotAllowed))
    );
    let quiet = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![
            Action::Native(NativeAction::new(NativeKind::Gmail, "{}").sending_no_mail()),
            Action::Redirect {
                to: vec![RuleAddress::new("a@example.com").unwrap()],
                copy: true,
            },
        ],
    );
    assert_eq!(one(&quiet, &target, EDIT).1.losses.len(), 1); // the approximated subject
}

/// Where a word match is the target's only on the subject and on the sender with its name, a word
/// match on any other address is not held, though the target holds word matches.
#[test]
fn a_word_match_is_held_only_on_the_subject_and_the_sender_with_its_name() {
    let target = search().with_limit(RuleLimit::WordsOnSubjectAndSender);
    let address = |field, part| Condition::Address {
        field,
        part,
        test: words("a"),
    };
    let held = discard(Match::all(vec![address(
        AddressField::From,
        AddressPart::WithName,
    )]));
    assert_eq!(one(&held, &target, EDIT).1.kept, Kept::Whole);
    for (field, part) in [
        (AddressField::From, AddressPart::All),
        (AddressField::ToOrCc, AddressPart::WithName),
        (AddressField::To, AddressPart::All),
        (AddressField::Cc, AddressPart::WithName),
    ] {
        let rule = discard(Match::all(vec![address(field, part)]));
        assert_eq!(
            one(&rule, &target, EDIT).1.kept,
            Kept::NotKept,
            "{field:?} {part:?}"
        );
        assert_eq!(
            one(&rule, &search(), EDIT).1.kept,
            Kept::Whole,
            "{field:?} {part:?}"
        );
    }
}
