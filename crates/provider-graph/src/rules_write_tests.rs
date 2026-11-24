//! A neutral rule written as a `messageRule` body: each cell of the Graph column the writer
//! places, and each it refuses because placing it would widen the rule.

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, Match, MeField,
        NativeAction, NativeCondition, NativeKind, RawRule, RuleAddress, RuleMailbox, RuleName,
        RulePart, RuleSyntax, TextOp, TextTest,
    },
};
use serde_json::{Map, Value, json};

use super::{body, clear_dropped, roles_needed, well_known};

fn test(op: TextOp, value: &str) -> TextTest {
    TextTest::new(op, vec![value.to_owned()]).unwrap()
}

fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: test(TextOp::Contains, value),
    }
}

fn from(part: AddressPart, op: TextOp, value: &str) -> Condition {
    Condition::Address {
        field: AddressField::From,
        part,
        test: test(op, value),
    }
}

fn read() -> Action {
    Action::AddKeywords(BTreeSet::from([Keyword::system(SystemKeyword::Seen)]))
}

fn junk(role: &MailboxRole) -> Option<MailboxId> {
    (*role == MailboxRole::Junk).then(|| MailboxId::try_from("junk-id").unwrap())
}

fn write(rule: &MailRule) -> Result<Map<String, Value>, RulePart> {
    body(rule, &junk)
}

#[test]
fn a_typed_rule_is_written_key_by_key() {
    let rule = MailRule::typed(
        Match::all(vec![
            subject("a"),
            Condition::Body(test(TextOp::Contains, "b")),
            from(AddressPart::All, TextOp::Is, "x@example.com"),
            Condition::Address {
                field: AddressField::ToOrCc,
                part: AddressPart::All,
                test: test(TextOp::Contains, "y"),
            },
            Condition::SentToMe(MeField::OnlyTo),
            Condition::HasAttachment,
            Condition::Importance(Importance::High),
        ])
        .except(vec![
            from(AddressPart::All, TextOp::Contains, "@example.org"),
            from(AddressPart::All, TextOp::Contains, "z"),
        ]),
        vec![
            Action::FileInto {
                mailbox: RuleMailbox::Id(MailboxId::try_from("f1").unwrap()),
                copy: true,
            },
            read(),
            Action::MarkImportance(Importance::Low),
            Action::Forward {
                to: vec![RuleAddress::new("f@example.com").unwrap()],
            },
        ],
    )
    .named(RuleName::new("Name").unwrap())
    .disabled()
    .stopping();
    let written = Value::Object(write(&rule).unwrap());
    assert_eq!(
        written,
        json!({
            "displayName": "Name",
            "isEnabled": false,
            "conditions": {
                "subjectContains": ["a"],
                "bodyContains": ["b"],
                "fromAddresses": [{"emailAddress": {"address": "x@example.com"}}],
                "recipientContains": ["y"],
                "sentOnlyToMe": true,
                "hasAttachments": true,
                "importance": "high"
            },
            // Any one exception vetoes, so two that write one key merge.
            "exceptions": {"senderContains": ["@example.org", "z"]},
            "actions": {
                "copyToFolder": "f1",
                "markAsRead": true,
                "markImportance": "low",
                "forwardTo": [{"emailAddress": {"address": "f@example.com"}}],
                "stopProcessingRules": true
            }
        })
    );
}

#[test]
fn two_conditions_on_one_key_are_refused_since_graph_would_read_either() {
    let rule = MailRule::typed(Match::all(vec![subject("a"), subject("b")]), vec![read()]);
    assert_eq!(write(&rule), Err(RulePart::Condition(1)));
    let two_senders = MailRule::typed(
        Match::all(vec![
            from(AddressPart::All, TextOp::Contains, "y"),
            from(AddressPart::All, TextOp::Contains, "z"),
        ]),
        vec![read()],
    );
    assert_eq!(write(&two_senders), Err(RulePart::Condition(1)));
    // The same condition twice says nothing more, and is written once.
    let twice = MailRule::typed(Match::all(vec![subject("a"), subject("a")]), vec![read()]);
    assert!(write(&twice).is_ok());
}

#[test]
fn an_address_graph_cannot_name_alone_is_refused() {
    for (field, part, op) in [
        (AddressField::To, AddressPart::All, TextOp::Contains),
        (AddressField::Cc, AddressPart::All, TextOp::Is),
        (AddressField::From, AddressPart::Domain, TextOp::Contains),
        // `@example.org` within the sender would also match `@example.org.evil.test`.
        (AddressField::From, AddressPart::Domain, TextOp::Is),
        (AddressField::From, AddressPart::LocalPart, TextOp::Is),
        (AddressField::From, AddressPart::All, TextOp::Matches),
    ] {
        let condition = Condition::Address {
            field,
            part,
            test: test(op, "v"),
        };
        let rule = MailRule::typed(Match::all(vec![condition]), vec![read()]);
        assert_eq!(
            write(&rule),
            Err(RulePart::Condition(0)),
            "{field:?} {part:?} {op:?}"
        );
    }
}

#[test]
fn what_graph_has_no_predicate_for_is_refused_by_position() {
    for condition in [
        Condition::Header {
            field: HeaderField::named("X-Spam"),
            test: test(TextOp::Contains, "yes"),
        },
        Condition::Header {
            field: HeaderField::Subject,
            test: test(TextOp::Is, "a"),
        },
        Condition::Body(test(TextOp::Regex, "a")),
        Condition::Size(engine_core::rules::SizeBound::Over(10)),
        Condition::Native(NativeCondition::new(NativeKind::Gmail, "{}")),
        Condition::Native(NativeCondition::new(NativeKind::Graph, "[1]")),
    ] {
        let rule = MailRule::typed(Match::all(vec![]).except(vec![condition]), vec![read()]);
        assert_eq!(write(&rule), Err(RulePart::Exception(0)));
    }
    let any = MailRule::typed(Match::any(vec![subject("a"), subject("b")]), vec![read()]);
    assert_eq!(write(&any), Err(RulePart::Mode));
    let opaque = MailRule::opaque(RawRule::new(RuleSyntax::Sieve, "keep;"));
    assert_eq!(write(&opaque), Err(RulePart::Rule));
}

#[test]
fn filing_by_role_is_delete_for_trash_and_a_folder_for_the_rest() {
    let trash = Action::FileInto {
        mailbox: RuleMailbox::Role(MailboxRole::Trash),
        copy: false,
    };
    let to_junk = Action::FileInto {
        mailbox: RuleMailbox::Role(MailboxRole::Junk),
        copy: false,
    };
    let rule = MailRule::typed(Match::default(), vec![trash.clone()]);
    assert_eq!(write(&rule).unwrap()["actions"], json!({"delete": true}));
    assert_eq!(roles_needed(&rule).len(), 0);
    let rule = MailRule::typed(Match::default(), vec![to_junk]);
    assert_eq!(roles_needed(&rule), [MailboxRole::Junk]);
    assert_eq!(
        write(&rule).unwrap()["actions"],
        json!({"moveToFolder": "junk-id"})
    );
    // A role with no folder found, or none Graph names, is not written.
    let archive = Action::FileInto {
        mailbox: RuleMailbox::Role(MailboxRole::Archive),
        copy: true,
    };
    assert_eq!(
        write(&MailRule::typed(Match::default(), vec![read(), archive])),
        Err(RulePart::Action(1))
    );
    assert_eq!(well_known(&MailboxRole::Archive), Some("archive"));
    assert_eq!(well_known(&MailboxRole::Flagged), None);
    let by_path = Action::FileInto {
        mailbox: RuleMailbox::Path(engine_core::rules::MailboxPath::new("A/B").unwrap()),
        copy: false,
    };
    assert_eq!(
        write(&MailRule::typed(Match::default(), vec![by_path])),
        Err(RulePart::Action(0))
    );
    // Two moves to different folders cannot both be one rule's.
    let two = vec![
        Action::FileInto {
            mailbox: RuleMailbox::Id(MailboxId::try_from("a").unwrap()),
            copy: false,
        },
        Action::FileInto {
            mailbox: RuleMailbox::Id(MailboxId::try_from("b").unwrap()),
            copy: false,
        },
    ];
    assert_eq!(
        write(&MailRule::typed(Match::default(), two)),
        Err(RulePart::Action(1))
    );
}

#[test]
fn keywords_are_read_marks_and_categories_and_a_flag_is_refused() {
    let named = Action::AddKeywords(BTreeSet::from([
        Keyword::system(SystemKeyword::Seen),
        Keyword::new("project").unwrap(),
    ]));
    let native = Action::Native(
        NativeAction::new(NativeKind::Graph, r#"{"assignCategories":["Red"]}"#).sending_no_mail(),
    );
    let rule = MailRule::typed(Match::default(), vec![named, native]);
    assert_eq!(
        write(&rule).unwrap()["actions"],
        json!({"markAsRead": true, "assignCategories": ["project", "Red"]})
    );
    let flag = Action::AddKeywords(BTreeSet::from([Keyword::system(SystemKeyword::Flagged)]));
    assert_eq!(
        write(&MailRule::typed(Match::default(), vec![flag])),
        Err(RulePart::Action(0))
    );
}

#[test]
fn a_redirect_is_written_only_where_it_keeps_the_message() {
    let to = vec![RuleAddress::new("r@example.com").unwrap()];
    let keeping = Action::Redirect {
        to: to.clone(),
        copy: true,
    };
    let rule = MailRule::typed(Match::default(), vec![keeping, Action::Discard]);
    assert_eq!(
        write(&rule).unwrap()["actions"],
        json!({"redirectTo": [{"emailAddress": {"address": "r@example.com"}}], "permanentDelete": true})
    );
    let moving = Action::Redirect { to, copy: false };
    assert_eq!(
        write(&MailRule::typed(Match::default(), vec![moving])),
        Err(RulePart::Action(0))
    );
}

#[test]
fn a_patch_clears_what_the_old_rule_held_and_the_new_one_does_not() {
    let old = json!({
        "conditions": {"subjectContains": ["a"], "hasAttachments": true},
        "actions": {"moveToFolder": "f", "markAsRead": true},
        "exceptions": {"bodyContains": ["x"]}
    });
    let rule = MailRule::typed(Match::all(vec![subject("b")]), vec![Action::Discard]);
    let mut new = write(&rule).unwrap();
    clear_dropped(&mut new, &old);
    assert_eq!(
        new["conditions"],
        json!({"subjectContains": ["b"], "hasAttachments": false})
    );
    assert_eq!(
        new["actions"],
        json!({"permanentDelete": true, "moveToFolder": null, "markAsRead": false})
    );
    assert_eq!(new["exceptions"], json!({"bodyContains": null}));
}

#[test]
fn a_patch_keeps_the_display_names_the_old_rule_had() {
    let old = json!({"actions": {"forwardTo": [
        {"emailAddress": {"name": "Person 1", "address": "p1@example.com"}},
        {"emailAddress": {"name": "Person 2", "address": "p2@example.com"}}
    ]}});
    let rule = MailRule::typed(
        Match::default(),
        vec![Action::Forward {
            to: vec![
                RuleAddress::new("p2@example.com").unwrap(),
                RuleAddress::new("p3@example.com").unwrap(),
            ],
        }],
    );
    let mut new = write(&rule).unwrap();
    super::keep_names(&mut new, &old);
    assert_eq!(
        new["actions"]["forwardTo"],
        json!([
            {"emailAddress": {"name": "Person 2", "address": "p2@example.com"}},
            {"emailAddress": {"address": "p3@example.com"}}
        ])
    );
}
