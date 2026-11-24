use engine_core::{
    error::FailureClass,
    mail::{Keyword, MailboxRole},
    rules::{
        Action, Condition, FailedRule, Kept, Loss, LossReason, MailRule, Match, NativeAction,
        NativeKind, RuleAddress, RuleBody, RuleMailbox, RulePart, RuleSet, RuleWriteFailure,
        TextOp, TextTest,
    },
};
use engine_provider::KeywordName;
use serde_json::{Value, json};

use super::*;
use crate::{
    named_labels::NamedLabels,
    rules_fake::{Filters, client},
    rules_read::tests::{from, subject},
};

fn filter(id: &str, subject: &str) -> Value {
    json!({"id": id, "criteria": {"subject": subject}, "action": {"addLabelIds": ["TRASH"]}})
}

pub(super) fn three() -> Filters {
    Filters::holding(vec![
        filter("a", "one"),
        filter("b", "two"),
        filter("c", "three"),
    ])
}

pub(super) fn no_names() -> Labels {
    Labels::new(Vec::new())
}

pub(super) async fn read(client: &GoogleClient) -> RuleSet {
    Listing::read(client)
        .await
        .unwrap()
        .rule_set(&NamedLabels::default())
}

/// What the filter of `subject` that trashes reads as.
pub(super) fn trash(subject_value: &str) -> RuleBody {
    MailRule::typed(
        Match::all(vec![subject(&[subject_value])]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Trash),
            copy: false,
        }],
    )
    .body
}

pub(super) fn discard(subject_value: &str) -> RuleBody {
    MailRule::typed(
        Match::all(vec![subject(&[subject_value])]),
        vec![Action::Discard],
    )
    .body
}

fn forward(rule: &MailRule, to: &str) -> MailRule {
    let mut rule = rule.clone();
    if let RuleBody::Typed(typed) = &mut rule.body {
        typed.actions.push(Action::Redirect {
            to: vec![RuleAddress::new(to).unwrap()],
            copy: true,
        });
    }
    rule
}

pub(super) fn writes(state: &std::sync::Mutex<Filters>) -> Vec<(&'static str, String)> {
    state.lock().unwrap().writes.clone()
}

#[tokio::test]
async fn a_set_written_back_as_read_sends_nothing() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let receipt = replace(&client, &no_names(), &base, &base.rules)
        .await
        .unwrap();
    assert_eq!(writes(&state), Vec::new());
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(receipt.left_as_read, Vec::<usize>::new());
    assert!(receipt.version.is_none());
}

/// Gmail has no update: the new filter is stored first, and only then the one it replaces is
/// removed, so a failed create never leaves the account with neither.
#[tokio::test]
async fn a_changed_rule_is_created_before_the_filter_it_replaces_is_deleted() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[1].body = discard("new");
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(
        writes(&state),
        [("POST", "new".to_owned()), ("DELETE", "b".to_owned())]
    );
    assert_eq!(state.lock().unwrap().ids(), ["a", "c", "new4"]);
    assert_eq!(receipt.failures, Vec::new());
}

#[tokio::test]
async fn a_failed_create_keeps_the_filter_it_was_to_replace() {
    let mut filters = three();
    filters.failing.insert(("POST", "new".to_owned()), 500);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0].body = discard("new");
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(state.lock().unwrap().ids(), ["a", "b", "c"]);
    assert_eq!(
        receipt.failures,
        [RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::Retryable
        )]
    );
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert!(
        receipt.outcomes[0]
            .losses
            .contains(&Loss::new(RulePart::Rule, LossReason::NotWritten))
    );
    assert_eq!(receipt.left_as_read, [0]);
}

/// A rule edited into something Gmail cannot hold is not written, and the filter it was read as
/// is not deleted for it: the edit did not apply, which the receipt says, and is no failure.
#[tokio::test]
async fn a_rule_gmail_cannot_hold_leaves_the_filter_it_was_read_as() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    let regex = Condition::Header {
        field: engine_core::rules::HeaderField::Subject,
        test: TextTest::new(TextOp::Regex, vec!["^x".to_owned()]).unwrap(),
    };
    rules[0].body = MailRule::typed(Match::all(vec![regex]), vec![Action::Discard]).body;
    // Filing into Sent, which the plan refuses too.
    rules[1].body = MailRule::typed(
        Match::all(vec![from(&["x"])]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Sent),
            copy: false,
        }],
    )
    .body;
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(writes(&state), Vec::new());
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert_eq!(receipt.outcomes[1].kept, Kept::NotKept);
    assert_eq!(receipt.left_as_read, [0, 1]);
}

#[tokio::test]
async fn filters_left_out_are_deleted_last_and_one_already_gone_is_done() {
    let mut filters = three();
    // `b` is answered as gone already, which is the state asked for; `c` fails.
    filters.failing.insert(("DELETE", "b".to_owned()), 404);
    filters.failing.insert(("DELETE", "c".to_owned()), 503);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = vec![base.rules[0].clone()];
    rules.push(MailRule::typed(
        Match::all(vec![subject(&["n"])]),
        vec![Action::Discard],
    ));
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(
        writes(&state),
        [
            ("POST", "new".to_owned()),
            ("DELETE", "b".to_owned()),
            ("DELETE", "c".to_owned()),
        ]
    );
    assert_eq!(state.lock().unwrap().ids(), ["a", "b", "c", "new4"]);
    assert_eq!(
        receipt.failures,
        [RuleWriteFailure::new(
            FailedRule::Removed(2),
            FailureClass::Retryable
        )]
    );
}

#[tokio::test]
async fn an_old_filter_that_will_not_go_stays_beside_its_replacement_and_says_so() {
    let mut filters = three();
    filters.failing.insert(("DELETE", "a".to_owned()), 403);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0].body = discard("new");
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(state.lock().unwrap().ids(), ["a", "b", "c", "new4"]);
    assert_eq!(
        receipt.failures,
        [RuleWriteFailure::new(
            FailedRule::Removed(0),
            FailureClass::Permanent
        )]
    );
    assert_eq!(receipt.outcomes[0].kept, Kept::Reduced);
    let beside = Loss::new(RulePart::Rule, LossReason::LeftBeside);
    assert!(receipt.outcomes[0].losses.contains(&beside));
}

#[tokio::test]
async fn a_base_not_read_or_moved_since_is_refused_before_anything_is_sent() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let unread = RuleSet::new(base.rules.clone());
    let err = replace(&client, &no_names(), &unread, &base.rules)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    let mut forged = base.clone();
    forged.version = Some(engine_core::version::RuleSetVersion::new("0".repeat(64)));
    let err = replace(&client, &no_names(), &forged, &base.rules)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);

    state.lock().unwrap().list.push(filter("d", "four"));
    let err = replace(&client, &no_names(), &base, &base.rules)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert_eq!(writes(&state), Vec::new());
}

/// Gmail forwards only to an address it has verified: a forward to any other, typed or Gmail's
/// own, is lost before anything is sent, and the addresses are asked once per write, only where a
/// rule forwards.
#[tokio::test]
async fn a_forward_to_an_unverified_address_is_not_allowed() {
    let mut filters = three();
    filters.forwarding = vec![
        json!({"forwardingEmail": "Kept@Example.net", "verificationStatus": "accepted"}),
        json!({"forwardingEmail": "pending@example.net", "verificationStatus": "pending"}),
    ];
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0] = forward(&rules[0], "kept@example.net");
    rules[1] = forward(&rules[1], "pending@example.net");
    let native = Action::Native(NativeAction::new(
        NativeKind::Gmail,
        r#"{"forward":"nobody@example.net"}"#,
    ));
    rules[2].body = MailRule::typed(Match::all(vec![from(&["x"])]), vec![native]).body;
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.forwarding_reads, 1);
    // The verified forward is written; the pending one loses its forward, which leaves the
    // filter it was read as, unsent; the third, Gmail's own, had nothing else to do.
    assert_eq!(state.ids(), ["b", "c", "new4"]);
    let forwards: Vec<Option<&str>> = state
        .list
        .iter()
        .map(|f| f["action"]["forward"].as_str())
        .collect();
    assert_eq!(forwards, [None, None, Some("kept@example.net")]);
    assert_eq!(receipt.outcomes[1].kept, Kept::Reduced);
    let held_back = Loss::new(RulePart::Action(1), LossReason::ForwardNotAllowed);
    assert!(receipt.outcomes[1].losses.contains(&held_back));
    assert_eq!(receipt.outcomes[2].kept, Kept::NotKept);
    let held_back = Loss::new(RulePart::Action(0), LossReason::ForwardNotAllowed);
    assert!(receipt.outcomes[2].losses.contains(&held_back));
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(receipt.left_as_read, [2]);
}

#[tokio::test]
async fn forwarding_addresses_that_cannot_be_asked_leave_the_rule_unwritten() {
    let mut filters = three();
    filters
        .failing
        .insert(("GET", "forwarding".to_owned()), 500);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0] = forward(&rules[0], "kept@example.net");
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(writes(&state), Vec::new());
    assert_eq!(
        receipt.failures,
        [RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::Retryable
        )]
    );
    assert_eq!(receipt.left_as_read, [0]);
}

fn work_labels() -> Labels {
    Labels::new(vec![
        KeywordName::new(Keyword::new("$work").unwrap(), "Work tag").unwrap(),
    ])
}

fn tagged() -> MailRule {
    MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![Action::AddKeywords([Keyword::new("$work").unwrap()].into())],
    )
}

#[tokio::test]
async fn a_keyword_is_filed_under_the_label_named_for_it_made_where_missing() {
    let (client, state) = client(Filters::default());
    let base = read(&client).await;
    let receipt = replace(&client, &work_labels(), &base, &[tagged()])
        .await
        .unwrap();
    assert_eq!(receipt.failures, Vec::new());
    let state = state.lock().unwrap();
    assert_eq!(state.labels[0]["name"], "Work tag");
    assert_eq!(state.list[0]["action"]["addLabelIds"], json!(["Label_900"]));
}

/// A label made for a keyword of a filter Gmail then did not store is removed again: no orphan.
#[tokio::test]
async fn a_label_made_for_a_filter_not_stored_is_removed() {
    let mut filters = Filters::default();
    filters.failing.insert(("POST", "new".to_owned()), 400);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let receipt = replace(&client, &work_labels(), &base, &[tagged()])
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    let state = state.lock().unwrap();
    assert_eq!(state.labels.len(), 0);
    assert!(
        state
            .writes
            .contains(&("GET label", "Label_900".to_owned()))
    );
    assert!(
        state
            .writes
            .contains(&("DELETE label", "Label_900".to_owned()))
    );
}

/// A label made for a filter not stored, that another device's keyword write put on a message
/// meanwhile, is in use: it stays.
#[tokio::test]
async fn a_label_made_but_used_meanwhile_stays() {
    let mut filters = Filters::default();
    filters.failing.insert(("POST", "new".to_owned()), 400);
    filters.label_messages = 1;
    let (client, state) = client(filters);
    let base = read(&client).await;
    let receipt = replace(&client, &work_labels(), &base, &[tagged()])
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    let state = state.lock().unwrap();
    assert_eq!(state.labels.len(), 1);
    assert!(
        state
            .writes
            .contains(&("GET label", "Label_900".to_owned()))
    );
    assert!(
        !state
            .writes
            .iter()
            .any(|(method, _)| *method == "DELETE label")
    );
}
