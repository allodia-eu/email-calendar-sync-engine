use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, Match, NativeAction,
        NativeCondition, NativeKind, RawRule, RuleAddress, RuleMailbox, RulePart, RuleSyntax,
        SizeBound, TextOp, TextTest,
    },
};
use serde_json::{Value, json};

use super::*;
use crate::rules_read::{
    self,
    tests::{contains, from, named, subject},
};

fn labels() -> BTreeMap<Keyword, String> {
    BTreeMap::from([(Keyword::new("$work").unwrap(), "Label_77".to_owned())])
}

fn write(rule: &MailRule) -> Value {
    filter(rule, &labels()).expect("Gmail holds it")
}

/// What Gmail would list for `filter`, read back.
fn read_back(filter: &Value) -> MailRule {
    let mut listed = filter.clone();
    listed["id"] = json!("new");
    let mut rule = rules_read::rule(&listed, 1, 0, &named());
    rule.origin = None;
    rule
}

fn move_to(id: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(id).unwrap()),
        copy: false,
    }
}

fn into_role(role: MailboxRole, copy: bool) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Role(role),
        copy,
    }
}

fn keywords(names: &[Keyword]) -> Action {
    Action::AddKeywords(names.iter().cloned().collect::<BTreeSet<_>>())
}

fn address(text: &str) -> Vec<RuleAddress> {
    vec![RuleAddress::new(text).unwrap()]
}

fn native(raw: &str) -> Condition {
    Condition::Native(NativeCondition::new(NativeKind::Gmail, raw))
}

#[test]
fn every_typed_part_is_read_back_as_it_was_written() {
    let recipients = Condition::Address {
        field: AddressField::ToOrCc,
        part: AddressPart::All,
        test: contains(&["team@example.org"]),
    };
    let rules = [
        MailRule::typed(
            Match::all(vec![
                from(&["a@example.com", "b c"]),
                recipients,
                subject(&["status"]),
                Condition::HasAttachment,
                Condition::Size(SizeBound::Under(400)),
            ])
            .except(vec![
                from(&["boss@example.com"]),
                Condition::Size(SizeBound::Over(9)),
            ]),
            vec![
                move_to("Label_1"),
                keywords(&[
                    Keyword::system(SystemKeyword::Seen),
                    Keyword::system(SystemKeyword::Flagged),
                    Keyword::new("$work").unwrap(),
                ]),
                Action::MarkImportance(Importance::Low),
                Action::Redirect {
                    to: address("archive@example.net"),
                    copy: true,
                },
            ],
        ),
        MailRule::typed(
            Match::any(vec![
                from(&["a"]),
                subject(&["b"]),
                Condition::HasAttachment,
            ]),
            vec![into_role(MailboxRole::Trash, false)],
        ),
        // All of them must hold, so their order means nothing; it is read back as Gmail keeps
        // them, the criteria first and the query after.
        MailRule::typed(
            Match::all(vec![
                from(&["a"]),
                Condition::Size(SizeBound::Over(1)),
                from(&["b"]),
                Condition::Size(SizeBound::Under(9)),
            ]),
            vec![
                Action::FileInto {
                    mailbox: RuleMailbox::Id(MailboxId::try_from("Label_2").unwrap()),
                    copy: true,
                },
                Action::MarkImportance(Importance::High),
            ],
        ),
        MailRule::typed(
            Match::all(vec![subject(&["x"])]),
            vec![into_role(MailboxRole::Archive, false)],
        ),
        MailRule::typed(
            Match::all(vec![subject(&["x"])]),
            vec![into_role(MailboxRole::Junk, false)],
        ),
    ];
    for rule in rules {
        let written = write(&rule);
        assert_eq!(read_back(&written), rule, "{written}");
    }
}

#[test]
fn conditions_go_into_gmails_criteria_and_a_second_of_a_kind_into_the_query() {
    let rule = MailRule::typed(
        Match::all(vec![from(&["a"]), from(&["b"]), subject(&["s t"])]),
        vec![move_to("Label_1")],
    );
    assert_eq!(
        write(&rule),
        json!({
            "criteria": { "from": "a", "subject": "\"s t\"", "query": "from:b" },
            "action": { "addLabelIds": ["Label_1"], "removeLabelIds": ["INBOX"] },
        })
    );
}

#[test]
fn any_of_conditions_and_the_exceptions_are_searches_joined_by_or() {
    let rule = MailRule::typed(
        Match::any(vec![from(&["a"]), subject(&["b"])]).except(vec![Condition::HasAttachment]),
        vec![Action::Discard],
    );
    assert_eq!(
        write(&rule),
        json!({
            "criteria": { "query": "from:a OR subject:b", "negatedQuery": "has:attachment" },
            "action": { "addLabelIds": ["TRASH"] },
        })
    );
}

#[test]
fn native_parts_are_put_back_as_they_were_read() {
    let filter = json!({
        "criteria": { "from": "me", "query": "invoice", "excludeChats": true },
        "action": {
            "addLabelIds": ["CATEGORY_UPDATES"],
            "removeLabelIds": ["SPAM"],
            "somethingNew": { "x": 1 },
        },
    });
    assert_eq!(write(&read_back(&filter)), filter);
}

/// What the plan cannot see, and the writer must not write: the caller keeps the filter the rule
/// was read as.
#[test]
fn what_gmail_cannot_hold_as_said_names_its_part() {
    let rule = MailRule::typed;
    let quote = TextTest::new(TextOp::Contains, vec!["say \"hi\"".to_owned()]).unwrap();
    let quoted = Condition::Header {
        field: HeaderField::Subject,
        test: quote,
    };
    let x = || Match::all(vec![subject(&["x"])]);
    let cases = [
        (
            rule(x(), vec![into_role(MailboxRole::Trash, true)]),
            RulePart::Action(0),
        ),
        (
            rule(
                x(),
                vec![Action::Discard, into_role(MailboxRole::Sent, false)],
            ),
            RulePart::Action(1),
        ),
        (
            rule(x(), vec![Action::MarkImportance(Importance::Normal)]),
            RulePart::Action(0),
        ),
        (
            rule(x(), vec![keywords(&[Keyword::new("$unnamed").unwrap()])]),
            RulePart::Action(0),
        ),
        (
            rule(Match::all(vec![quoted]), vec![Action::Discard]),
            RulePart::Condition(0),
        ),
        (
            rule(
                Match::all(vec![
                    native(r#"{"query":"invoice"}"#),
                    from(&["a"]),
                    from(&["b"]),
                ]),
                vec![Action::Discard],
            ),
            RulePart::Condition(2),
        ),
        (
            rule(
                Match::any(vec![native(r#"{"query":"x"}"#), from(&["a"])]),
                vec![Action::Discard],
            ),
            RulePart::Condition(0),
        ),
        (
            rule(
                Match::all(vec![native(r#"{"negatedQuery":"x"}"#)]).except(vec![from(&["a"])]),
                vec![Action::Discard],
            ),
            RulePart::Exception(0),
        ),
        (
            rule(
                Match::all(vec![Condition::Native(NativeCondition::new(
                    NativeKind::Graph,
                    "{}",
                ))]),
                vec![Action::Discard],
            ),
            RulePart::Condition(0),
        ),
        (
            rule(
                x(),
                vec![Action::Native(NativeAction::new(NativeKind::Graph, "{}"))],
            ),
            RulePart::Action(0),
        ),
        (
            rule(
                x(),
                vec![
                    Action::Forward {
                        to: address("a@example.com"),
                    },
                    Action::Redirect {
                        to: address("b@example.com"),
                        copy: true,
                    },
                ],
            ),
            RulePart::Action(1),
        ),
    ];
    for (rule, part) in cases {
        assert_eq!(filter(&rule, &labels()), Err(part), "{rule:?}");
    }
    let opaque = MailRule::opaque(RawRule::new(RuleSyntax::Sieve, "keep;"));
    assert_eq!(filter(&opaque, &labels()), Err(RulePart::Rule));
}
