use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, Condition, HeaderField, MailRule, Match, NativeAction, NativeCondition, NativeKind,
        RawRule, RuleAddress, RuleMailbox, RulePart, RuleSyntax, SizeBound, TextOp, TextTest,
    },
};
use serde_json::{Value, json};

use super::*;
use crate::rules_read::{
    self,
    tests::{from, named, subject},
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
    let rules = [
        MailRule::typed(
            Match::all(vec![
                from(&["a@example.com", "b c"]),
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
            vec![
                into_role(MailboxRole::Archive, false),
                keywords(&[Keyword::new("$work").unwrap()]),
            ],
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

/// A typed condition that finds its criterion taken goes into the search, after a native one held
/// there in brackets, so that it stays one term; the exceptions likewise, after a native
/// `negatedQuery`. A native `hasAttachment` is never overwritten.
#[test]
fn a_typed_condition_beside_a_native_search_goes_after_it_in_brackets() {
    let rule = MailRule::typed(
        Match::all(vec![
            from(&["a"]),
            native(r#"{"query":"invoice OR receipt"}"#),
            from(&["b"]),
            native(r#"{"negatedQuery":"x y"}"#),
            native(r#"{"hasAttachment":"maybe"}"#),
            Condition::HasAttachment,
        ])
        .except(vec![subject(&["z"])]),
        vec![Action::Discard],
    );
    assert_eq!(
        write(&rule),
        json!({
            "criteria": {
                "from": "a",
                "query": "(invoice OR receipt) from:b has:attachment",
                "negatedQuery": "(x y) OR subject:z",
                "hasAttachment": "maybe",
            },
            "action": { "addLabelIds": ["TRASH"] },
        })
    );
    // A native `from` holds the field: the typed one goes into the search.
    let me = MailRule::typed(
        Match::all(vec![from(&["a"]), native(r#"{"from":"me"}"#)]),
        vec![Action::Discard],
    );
    assert_eq!(
        write(&me)["criteria"],
        json!({"from": "me", "query": "from:a"})
    );
}

/// What the writer refuses: each a part the plan refuses first (Gmail's vocabulary and its
/// limits), and the few it cannot see (two native parts with one key, another provider's own
/// part). The caller keeps the filter the rule was read as.
#[test]
fn what_gmail_cannot_hold_as_said_names_its_part() {
    let rule = MailRule::typed;
    let quote = TextTest::new(TextOp::Words, vec!["say \"hi\"".to_owned()]).unwrap();
    let quoted = Condition::Header {
        field: HeaderField::Subject,
        test: quote,
    };
    let x = || Match::all(vec![subject(&["x"])]);
    let forward = |to: &str| Action::Forward { to: address(to) };
    let native_forward = Action::Native(NativeAction::new(
        NativeKind::Gmail,
        r#"{"forward":"b@example.com"}"#,
    ));
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
                Match::any(vec![native(r#"{"query":"x"}"#), from(&["a"])]),
                vec![Action::Discard],
            ),
            RulePart::Condition(0),
        ),
        (
            rule(
                Match::all(vec![native(r#"{"query":"x"}"#), native(r#"{"query":"y"}"#)]),
                vec![Action::Discard],
            ),
            RulePart::Condition(1),
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
                    forward("a@example.com"),
                    Action::Redirect {
                        to: address("b@example.com"),
                        copy: true,
                    },
                ],
            ),
            RulePart::Action(1),
        ),
        // A typed forward and Gmail's own: a second forward is refused, never one dropped.
        (
            rule(x(), vec![forward("a@example.com"), native_forward.clone()]),
            RulePart::Action(1),
        ),
        (
            rule(x(), vec![native_forward, forward("a@example.com")]),
            RulePart::Action(1),
        ),
        (
            rule(
                x(),
                vec![Action::Native(NativeAction::new(
                    NativeKind::Gmail,
                    r#"{"forward":1}"#,
                ))],
            ),
            RulePart::Action(0),
        ),
    ];
    for (rule, part) in cases {
        assert_eq!(filter(&rule, &labels()), Err(part), "{rule:?}");
    }
    let opaque = MailRule::opaque(RawRule::new(RuleSyntax::Sieve, "keep;"));
    assert_eq!(filter(&opaque, &labels()), Err(RulePart::Rule));
}
