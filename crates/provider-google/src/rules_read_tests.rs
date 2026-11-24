use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, Match, MatchMode,
        NativeKind, RuleAddress, RuleBody, RuleMailbox, RuleOrigin, SizeBound, TextOp, TextTest,
    },
};
use engine_provider::KeywordName;
use serde_json::{Value, json};

use super::*;

/// The labels a host named for a keyword: `Label_77` stands for `$work`.
pub(crate) fn named() -> NamedLabels {
    let names = [KeywordName::new(Keyword::new("$work").unwrap(), "Work tag").unwrap()];
    NamedLabels::from_list(&[json!({"id": "Label_77", "name": "Work tag"})], &names)
}

fn read(filter: &Value) -> MailRule {
    rule(filter, 7, 0, &named())
}

fn typed(rule: &MailRule) -> (&Match, &[Action]) {
    let RuleBody::Typed(typed) = &rule.body else {
        panic!("a filter reads typed");
    };
    (&typed.when, &typed.actions)
}

pub(crate) fn contains(values: &[&str]) -> TextTest {
    TextTest::new(
        TextOp::Contains,
        values.iter().map(|value| (*value).to_owned()).collect(),
    )
    .unwrap()
}

pub(crate) fn from(values: &[&str]) -> Condition {
    Condition::Address {
        field: AddressField::From,
        part: AddressPart::All,
        test: contains(values),
    }
}

pub(crate) fn subject(values: &[&str]) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: contains(values),
    }
}

fn native(raw: &str) -> Condition {
    Condition::Native(NativeCondition::new(NativeKind::Gmail, raw))
}

fn native_action(raw: &str) -> Action {
    Action::Native(NativeAction::new(NativeKind::Gmail, raw).sending_no_mail())
}

fn label(id: &str, copy: bool) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(id).unwrap()),
        copy,
    }
}

fn role(role: MailboxRole) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Role(role),
        copy: false,
    }
}

#[test]
fn criteria_read_as_typed_conditions_all_of_which_hold() {
    let rule = read(&json!({
        "id": "f1",
        "criteria": {
            "from": "a@example.com OR b@example.com",
            "to": "team@example.org",
            "subject": "\"weekly report\"",
            "hasAttachment": true,
            "size": 1_000_000,
            "sizeComparison": "larger",
        },
        "action": { "addLabelIds": ["STARRED"] },
    }));
    let (when, _) = typed(&rule);
    assert_eq!(when.mode, MatchMode::All);
    assert_eq!(
        when.conditions,
        [
            from(&["a@example.com", "b@example.com"]),
            Condition::Address {
                field: AddressField::ToOrCc,
                part: AddressPart::All,
                test: contains(&["team@example.org"]),
            },
            subject(&["weekly report"]),
            Condition::HasAttachment,
            Condition::Size(SizeBound::Over(1_000_000)),
        ]
    );
    assert_eq!(when.except.len(), 0);
    assert_eq!(rule.origin, Some(RuleOrigin::Listed { set: 7, index: 0 }));
    assert!(rule.enabled && rule.name.is_none());
}

#[test]
fn a_query_of_the_subset_adds_its_terms_and_any_other_stays_whole() {
    let rule = read(&json!({
        "criteria": { "from": "a@example.com", "query": "subject:x smaller:500" },
        "action": { "addLabelIds": ["STARRED"] },
    }));
    assert_eq!(
        typed(&rule).0.conditions,
        [
            from(&["a@example.com"]),
            subject(&["x"]),
            Condition::Size(SizeBound::Under(500)),
        ]
    );

    let rule = read(&json!({
        "criteria": { "from": "me", "query": "invoice OR receipt", "excludeChats": true },
        "action": { "addLabelIds": ["STARRED"] },
    }));
    assert_eq!(
        typed(&rule).0.conditions,
        [
            native(r#"{"from":"me"}"#),
            native(r#"{"excludeChats":true}"#),
            native(r#"{"query":"invoice OR receipt"}"#),
        ]
    );
}

#[test]
fn a_query_of_terms_joined_by_or_alone_is_an_any_of_rule() {
    let rule = read(&json!({
        "criteria": { "query": "from:a@example.com OR subject:x" },
        "action": { "addLabelIds": ["TRASH"] },
    }));
    let (when, _) = typed(&rule);
    assert_eq!(when.mode, MatchMode::Any);
    assert_eq!(when.conditions, [from(&["a@example.com"]), subject(&["x"])]);

    // Beside another criterion it would be read as one more of them: it stays whole.
    let rule = read(&json!({
        "criteria": { "to": "b@example.com", "query": "from:a@example.com OR subject:x" },
        "action": { "addLabelIds": ["TRASH"] },
    }));
    let (when, _) = typed(&rule);
    assert_eq!(when.mode, MatchMode::All);
    assert_eq!(
        when.conditions[1],
        native(r#"{"query":"from:a@example.com OR subject:x"}"#)
    );
}

/// `negatedQuery` vetoes the filter where it matches: `a OR b` there is two exceptions, any one
/// of which vetoes; `a b` vetoes only where both hold, which no list of exceptions says.
#[test]
fn a_negated_query_is_the_exceptions_only_where_it_is_an_or() {
    let rule = read(&json!({
        "criteria": { "subject": "x", "negatedQuery": "from:boss@example.com OR has:attachment" },
        "action": { "addLabelIds": ["TRASH"] },
    }));
    let (when, _) = typed(&rule);
    assert_eq!(
        when.except,
        [from(&["boss@example.com"]), Condition::HasAttachment]
    );

    let rule = read(&json!({
        "criteria": { "subject": "x", "negatedQuery": "from:boss@example.com has:attachment" },
        "action": { "addLabelIds": ["TRASH"] },
    }));
    let (when, _) = typed(&rule);
    assert_eq!(when.except.len(), 0);
    assert_eq!(
        when.conditions,
        [
            subject(&["x"]),
            native(r#"{"negatedQuery":"from:boss@example.com has:attachment"}"#),
        ]
    );
}

#[test]
fn a_criterion_not_as_documented_stays_whole_and_one_asking_nothing_is_left_out() {
    let rule = read(&json!({
        "criteria": {
            "from": "",
            "hasAttachment": false,
            "excludeChats": false,
            "size": 10,
            "sizeComparison": "unspecified",
            "subject": 5,
        },
        "action": { "addLabelIds": ["STARRED"] },
    }));
    assert_eq!(
        typed(&rule).0.conditions,
        [
            native(r#"{"subject":5}"#),
            native(r#"{"size":10,"sizeComparison":"unspecified"}"#),
        ]
    );
}

#[test]
fn a_label_added_files_the_message_moved_where_the_inbox_is_removed() {
    let moved = read(&json!({
        "criteria": { "from": "a" },
        "action": { "addLabelIds": ["Label_1", "Label_2"], "removeLabelIds": ["INBOX"] },
    }));
    assert_eq!(
        typed(&moved).1,
        [label("Label_1", false), label("Label_2", false)]
    );
    let copied = read(&json!({
        "criteria": { "from": "a" },
        "action": { "addLabelIds": ["Label_1"] },
    }));
    assert_eq!(typed(&copied).1, [label("Label_1", true)]);
}

#[test]
fn trash_spam_and_the_inbox_removed_are_roles() {
    let cases = [
        (
            json!({ "addLabelIds": ["TRASH"] }),
            vec![role(MailboxRole::Trash)],
        ),
        (
            json!({ "addLabelIds": ["SPAM"] }),
            vec![role(MailboxRole::Junk)],
        ),
        (
            json!({ "removeLabelIds": ["INBOX"] }),
            vec![role(MailboxRole::Archive)],
        ),
        (
            json!({ "addLabelIds": ["TRASH"], "removeLabelIds": ["INBOX"] }),
            vec![role(MailboxRole::Trash)],
        ),
    ];
    for (action, expected) in cases {
        let rule = read(&json!({ "criteria": { "from": "a" }, "action": action }));
        assert_eq!(typed(&rule).1, expected.as_slice(), "{action}");
    }
}

#[test]
fn marks_are_keywords_and_gmails_importance_marker() {
    let rule = read(&json!({
        "criteria": { "from": "a" },
        "action": { "addLabelIds": ["STARRED", "Label_77", "IMPORTANT"], "removeLabelIds": ["UNREAD"] },
    }));
    let keywords: BTreeSet<Keyword> = [
        Keyword::system(SystemKeyword::Flagged),
        Keyword::system(SystemKeyword::Seen),
        Keyword::new("$work").unwrap(),
    ]
    .into();
    assert_eq!(
        typed(&rule).1,
        [
            Action::AddKeywords(keywords),
            Action::MarkImportance(Importance::High),
        ]
    );
    let never = read(&json!({
        "criteria": { "from": "a" },
        "action": { "removeLabelIds": ["IMPORTANT"] },
    }));
    assert_eq!(typed(&never).1, [Action::MarkImportance(Importance::Low)]);
    // Both at once says nothing a mark could: each stays as it was.
    let both = read(&json!({
        "criteria": { "from": "a" },
        "action": { "addLabelIds": ["IMPORTANT"], "removeLabelIds": ["IMPORTANT"] },
    }));
    assert_eq!(
        typed(&both).1,
        [
            native_action(r#"{"addLabelIds":["IMPORTANT"]}"#),
            native_action(r#"{"removeLabelIds":["IMPORTANT"]}"#),
        ]
    );
}

#[test]
fn a_forward_is_a_redirect_that_keeps_the_message() {
    let rule = read(&json!({
        "criteria": { "from": "a" },
        "action": { "forward": "archive@example.net" },
    }));
    assert_eq!(
        typed(&rule).1,
        [Action::Redirect {
            to: vec![RuleAddress::new("archive@example.net").unwrap()],
            copy: true,
        }]
    );
}

#[test]
fn every_other_label_change_is_native_and_sends_no_mail() {
    let rule = read(&json!({
        "criteria": { "from": "a" },
        "action": {
            "addLabelIds": ["CATEGORY_PROMOTIONS", "INBOX", "UNREAD"],
            "removeLabelIds": ["SPAM", "STARRED"],
            "somethingNew": {"x": 1},
        },
    }));
    let (_, actions) = typed(&rule);
    assert_eq!(
        actions[..5],
        [
            native_action(r#"{"addLabelIds":["CATEGORY_PROMOTIONS"]}"#),
            native_action(r#"{"addLabelIds":["INBOX"]}"#),
            native_action(r#"{"addLabelIds":["UNREAD"]}"#),
            native_action(r#"{"removeLabelIds":["SPAM"]}"#),
            native_action(r#"{"removeLabelIds":["STARRED"]}"#),
        ]
    );
    // An action of a kind this reader has never seen may send mail.
    let Action::Native(unknown) = &actions[5] else {
        panic!("native");
    };
    assert!(unknown.sends_mail);
}

#[test]
fn a_label_id_that_is_no_users_label_is_not_a_folder() {
    assert!(is_user_label("Label_12"));
    for id in ["Label_", "LABEL_1", "INBOX", "label_1", "Lab"] {
        assert!(!is_user_label(id), "{id}");
    }
}

const CAPTURED: &str = include_str!("../tests/fixtures/mail/settings_filters.json");

/// Two filters the live suite wrote through the engine, as Gmail listed them: read back typed,
/// each part as written.
#[test]
fn captured_filters_read_back_typed() {
    let listed: Value = serde_json::from_str(CAPTURED).unwrap();
    let filters = listed["filter"].as_array().unwrap();
    let first = rule(&filters[0], 1, 0, &named());
    let (when, actions) = typed(&first);
    assert_eq!(
        when.conditions,
        [from(&["a@example.com"]), subject(&["engine-live-filter-a"])]
    );
    let marks: BTreeSet<Keyword> = [
        Keyword::system(SystemKeyword::Flagged),
        Keyword::system(SystemKeyword::Seen),
    ]
    .into();
    assert_eq!(actions, [Action::AddKeywords(marks)]);
    let second = rule(&filters[1], 1, 1, &named());
    let (when, actions) = typed(&second);
    assert_eq!(
        when.conditions,
        [
            subject(&["engine-live-filter-b1", "engine-live-filter-b2"]),
            Condition::Size(SizeBound::Over(5_000_000)),
        ]
    );
    assert_eq!(
        when.except,
        [Condition::HasAttachment, from(&["boss@example.com"])]
    );
    assert_eq!(
        actions,
        [
            role(MailboxRole::Archive),
            Action::MarkImportance(Importance::High)
        ]
    );
}
