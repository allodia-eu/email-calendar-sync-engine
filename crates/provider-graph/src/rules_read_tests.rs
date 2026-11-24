//! `messageRule`s read into the model, one mapping cell of the design note's Graph column per
//! assertion, against the scrubbed capture (`message_rules.json`) and the shapes the capture
//! lacks (`message_rules_shapes.json`, written after Graph's reference in the capture's shape).

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, ForwardingRules, HeaderField, Kept, MailRule,
        MeField, NativeKind, PlanMode, RuleBody, RuleFeature, RuleMailbox, RuleName, RuleOrigin,
        RuleVocabulary, TextOp, TypedRule, plan_rules,
    },
};
use serde_json::Value;

use crate::{rules::vocabulary, test_support::json};

pub(crate) const CAPTURED: &str = include_str!("../tests/fixtures/rules/message_rules.json");
pub(crate) const SHAPES: &str = include_str!("../tests/fixtures/rules/message_rules_shapes.json");

pub(crate) fn listed(fixture: &str) -> Vec<MailRule> {
    let doc = json(fixture);
    doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, value)| super::rule(value, 7, index))
        .collect()
}

fn typed(rule: &MailRule) -> &TypedRule {
    match &rule.body {
        RuleBody::Typed(typed) => typed,
        _ => panic!("a Graph rule reads typed"),
    }
}

fn address(field: AddressField, op: TextOp, value: &str) -> impl Fn(&Condition) -> bool {
    move |condition| {
        matches!(condition, Condition::Address { field: f, part: AddressPart::All, test }
            if *f == field && test.op() == op && test.values() == [value.to_owned()])
    }
}

#[test]
fn the_captured_rules_read_typed_with_their_order_name_and_switch() {
    let rules = listed(CAPTURED);
    assert_eq!(rules.len(), 9);
    let first = &rules[0];
    assert_eq!(
        first.name.as_ref().map(RuleName::as_str),
        Some("Example rule 1")
    );
    assert!(first.enabled && !first.in_error && !first.read_only);
    assert_eq!(first.origin, Some(RuleOrigin::Listed { set: 7, index: 0 }));
    let body = typed(first);
    assert!(body.stop);
    assert!(
        matches!(&body.when.conditions[..], [Condition::Header { field: HeaderField::Subject, test }]
        if test.op() == TextOp::Contains && test.values() == ["marker-subject-1".to_owned()])
    );
    let folder = json(CAPTURED)["value"][0]["actions"]["moveToFolder"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.actions.contains(&Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(folder.as_str()).unwrap()),
        copy: false,
    }));
    assert!(
        body.actions
            .contains(&Action::AddKeywords(BTreeSet::from([Keyword::system(
                SystemKeyword::Seen
            )])))
    );
    assert!(
        body.actions
            .iter()
            .any(|a| matches!(a, Action::Forward { to } if to.len() == 1))
    );
    // Two subject values in one predicate are any-of: one test with both.
    let second = typed(&rules[1]);
    assert!(
        matches!(&second.when.conditions[..], [Condition::Body(_), Condition::Header { test, .. }]
        if test.values().len() == 2)
    );
    let enabled: Vec<bool> = rules.iter().map(|rule| rule.enabled).collect();
    assert_eq!(
        enabled,
        [true, true, true, true, true, false, false, false, false]
    );
}

#[test]
fn senders_recipients_and_exceptions_read_as_address_tests() {
    let rules = listed(CAPTURED);
    // `fromAddresses` is the sender's address, by equality.
    assert!(typed(&rules[5]).when.conditions.iter().any(address(
        AddressField::From,
        TextOp::Is,
        "person2@example.com"
    )));
    // `senderContains`, by containment; its `fromAddresses` exception vetoes.
    let eighth = typed(&rules[7]);
    assert!(eighth.when.conditions.iter().any(address(
        AddressField::From,
        TextOp::Contains,
        "@sender1.example.com"
    )));
    assert_eq!(eighth.when.except.len(), 1);
    let shapes = listed(SHAPES);
    let first = typed(&shapes[0]);
    assert!(first.when.conditions.iter().any(address(
        AddressField::ToOrCc,
        TextOp::Contains,
        "marker-recipient"
    )));
    assert!(first.when.conditions.iter().any(address(
        AddressField::ToOrCc,
        TextOp::Is,
        "person2@example.com"
    )));
    assert!(first.when.except.iter().any(address(
        AddressField::From,
        TextOp::Contains,
        "@sender.example.com"
    )));
    assert!(first.when.except.contains(&Condition::HasAttachment));
}

#[test]
fn every_typed_predicate_of_graph_reads_as_its_cell_says() {
    let shapes = listed(SHAPES);
    let first = typed(&shapes[0]);
    let conditions = &first.when.conditions;
    for me in [
        MeField::To,
        MeField::Cc,
        MeField::ToOrCc,
        MeField::OnlyTo,
        MeField::NotTo,
    ] {
        assert!(conditions.contains(&Condition::SentToMe(me)), "{me:?}");
    }
    assert!(conditions.contains(&Condition::Importance(Importance::High)));
    assert!(
        conditions
            .iter()
            .any(|c| matches!(c, Condition::Body(t) if t.op() == TextOp::Contains))
    );
    // `isAutomaticReply: false` asks for nothing, and is not read.
    assert!(!conditions.iter().any(|c| matches!(c, Condition::Native(_))));
    assert_eq!(conditions.len(), 9);
    assert!(first.actions.iter().any(|a| matches!(
        a,
        Action::FileInto {
            mailbox: RuleMailbox::Id(_),
            copy: true
        }
    )));
    assert!(
        first
            .actions
            .contains(&Action::MarkImportance(Importance::Low))
    );
    // `delete: false` asks for nothing.
    assert_eq!(first.actions.len(), 2);
}

#[test]
fn what_the_model_has_no_type_for_is_carried_native_one_predicate_each() {
    let shapes = listed(SHAPES);
    let second = &shapes[1];
    assert!(second.in_error && second.read_only);
    let body = typed(second);
    let natives: Vec<Value> = body
        .when
        .conditions
        .iter()
        .map(|c| match c {
            Condition::Native(native) => {
                assert_eq!(native.kind, NativeKind::Graph);
                serde_json::from_str(&native.raw).unwrap()
            }
            other => panic!("typed: {other:?}"),
        })
        .collect();
    let keys: Vec<&str> = natives
        .iter()
        .map(|n| n.as_object().unwrap().keys().next().unwrap().as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "bodyOrSubjectContains",
            "categories",
            "headerContains",
            "isAutomaticReply",
            "isMeetingRequest",
            "messageActionFlag",
            "sensitivity",
            "withinSizeRange"
        ]
    );
    assert!(natives.iter().all(|n| n.as_object().unwrap().len() == 1));
    assert!(matches!(&body.when.except[..], [Condition::Native(_)]));
    // `delete` is a move to Trash; forwarding as an attachment is native and sends mail.
    assert!(body.actions.contains(&Action::FileInto {
        mailbox: RuleMailbox::Role(MailboxRole::Trash),
        copy: false
    }));
    assert!(
        body.actions
            .iter()
            .any(|a| matches!(a, Action::Native(n) if n.sends_mail))
    );
}

#[test]
fn categories_read_as_keywords_only_where_each_is_one_as_written() {
    // The capture's category holds a space and capitals: native, and it sends no mail.
    let captured = listed(CAPTURED);
    let ninth = typed(&captured[8]);
    assert!(
        ninth
            .actions
            .iter()
            .any(|a| matches!(a, Action::Native(n) if !n.sends_mail))
    );
    let third = &listed(SHAPES)[2];
    let body = typed(third);
    assert!(!third.enabled && !body.stop);
    assert_eq!(body.when.conditions.len(), 0);
    assert!(body.actions.contains(&Action::Discard));
    assert!(body.actions.contains(&Action::AddKeywords(BTreeSet::from([
        Keyword::new("alpha").unwrap(),
        Keyword::new("beta").unwrap()
    ]))));
}

#[test]
fn a_redirect_reads_as_one_that_keeps_the_message() {
    let captured = listed(CAPTURED);
    let fifth = typed(&captured[4]);
    assert!(
        fifth
            .actions
            .iter()
            .any(|a| matches!(a, Action::Redirect { copy: true, to } if to.len() == 1))
    );
    assert!(fifth.when.conditions.contains(&Condition::HasAttachment));
}

#[test]
fn every_rule_graph_reads_is_one_graph_holds_whole() {
    let rules: Vec<MailRule> = listed(CAPTURED).into_iter().chain(listed(SHAPES)).collect();
    let plan = plan_rules(&rules, &vocabulary(), PlanMode::Edit);
    for outcome in &plan.outcomes {
        assert_eq!(outcome.kept, Kept::Whole, "{outcome:?}");
    }
    // And the writer places every part of every one.
    for rule in &rules {
        crate::rules_write::body(rule, &|_| Some(MailboxId::try_from("folder").unwrap()))
            .unwrap_or_else(|part| panic!("{part:?}"));
    }
}

#[test]
fn graph_rules_planned_for_import_keep_both_safety_rules() {
    let rules = listed(CAPTURED);
    let import = PlanMode::Import {
        forwarding: ForwardingRules::Disabled,
    };
    // A target that holds every part but exceptions and Graph's own.
    let target = RuleVocabulary::new().with_exact([
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::Body(TextOp::Contains),
        RuleFeature::AddressText(TextOp::Is),
        RuleFeature::AddressText(TextOp::Contains),
        RuleFeature::HasAttachment,
        RuleFeature::FileIntoId,
        RuleFeature::RedirectCopy,
        RuleFeature::Forward,
        RuleFeature::MarkRead,
        RuleFeature::Stop,
        RuleFeature::Disabled,
        RuleFeature::Name,
        RuleFeature::Order,
    ]);
    let plan = plan_rules(&rules, &target, import);
    // Forwarding and redirecting rules arrive disabled for review.
    assert_eq!(plan.outcomes[0].kept, Kept::Disabled);
    assert_eq!(plan.outcomes[4].kept, Kept::Disabled);
    // The rule whose exception the target cannot hold is not kept: losing it would widen it.
    assert_eq!(plan.outcomes[7].kept, Kept::NotKept);
    // An ordinary rule is carried whole.
    assert_eq!(plan.outcomes[1].kept, Kept::Whole);
}

#[test]
fn debug_of_the_rules_read_shows_no_value() {
    // A folder id is an identifier, shown as every `MailboxId` is; the values a rule tests and
    // sends to, its name and its categories are content.
    let shown = format!("{:?}{:?}", listed(CAPTURED), listed(SHAPES));
    for value in [
        "marker",
        "example.com",
        "Example rule",
        "Example Category",
        "Typed predicates",
        "alpha",
    ] {
        assert!(!shown.contains(value), "{value} leaked");
    }
}

#[test]
fn a_part_that_is_not_what_graph_documents_is_carried_native() {
    let odd = serde_json::json!({
        "id": "x",
        "conditions": { "subjectContains": [1], "fromAddresses": [{ "emailAddress": {} }] },
        "actions": {
            "moveToFolder": 5,
            "redirectTo": [{}],
            "markImportance": "urgent",
            "futureAction": true
        }
    });
    let rule = super::rule(&odd, 1, 0);
    assert!(rule.name.is_none() && rule.enabled);
    let body = typed(&rule);
    assert!(
        body.when
            .conditions
            .iter()
            .all(|c| matches!(c, Condition::Native(_)))
    );
    assert_eq!(body.when.conditions.len(), 2);
    assert!(
        body.actions
            .iter()
            .all(|a| matches!(a, Action::Native(n) if n.sends_mail))
    );
    assert_eq!(body.actions.len(), 4);
}
