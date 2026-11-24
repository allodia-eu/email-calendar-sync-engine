use engine_core::{
    mail::{Keyword, MailboxRole},
    rules::{Condition, Kept, Loss, LossReason, Match, RuleAddress, RuleMailbox, TextOp, TextTest},
};
use engine_provider::KeywordName;
use serde_json::json;

use super::*;
use crate::{
    rules_fake::{Filters, client},
    rules_read::tests::{from, subject},
};

fn filter(id: &str, from: &str) -> Value {
    json!({"id": id, "criteria": {"from": from}, "action": {"addLabelIds": ["TRASH"]}})
}

fn three() -> Filters {
    Filters::holding(vec![
        filter("a", "one@example.com"),
        filter("b", "two@example.com"),
        filter("c", "three@example.com"),
    ])
}

fn no_names() -> Labels {
    Labels::new(Vec::new())
}

async fn read(client: &GoogleClient) -> RuleSet {
    Listing::read(client)
        .await
        .unwrap()
        .rule_set(&NamedLabels::default())
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

#[tokio::test]
async fn a_set_written_back_as_read_sends_nothing() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let receipt = replace(&client, &no_names(), &base, &base.rules)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().writes.len(), 0);
    assert_eq!(receipt.failures.len(), 0);
    assert!(receipt.version.is_none());
}

/// Gmail has no update: the new filter is stored first, and only then the one it replaces is
/// removed, so a failed create never leaves the account with neither.
#[tokio::test]
async fn a_changed_rule_is_created_before_the_filter_it_replaces_is_deleted() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[1].body =
        MailRule::typed(Match::all(vec![subject(&["new"])]), vec![Action::Discard]).body;
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!(
        state.writes,
        [("POST", "new".to_owned()), ("DELETE", "b".to_owned())]
    );
    assert_eq!(state.ids(), ["a", "c", "new4"]);
    assert_eq!(receipt.failures.len(), 0);
}

#[tokio::test]
async fn a_failed_create_keeps_the_filter_it_was_to_replace() {
    let mut filters = three();
    filters.failing.insert(("POST", "new".to_owned()), 500);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0].body =
        MailRule::typed(Match::all(vec![subject(&["new"])]), vec![Action::Discard]).body;
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
}

/// A rule edited into something Gmail cannot hold is not written, and the filter it was read as
/// is not deleted for it: the write never removes a rule it then refuses to write.
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
    // A role Gmail keeps as no label: the plan lets it through, the writer cannot place it.
    rules[1].body = MailRule::typed(
        Match::all(vec![from(&["x"])]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Sent),
            copy: false,
        }],
    )
    .body;
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(state.lock().unwrap().writes.len(), 0);
    assert_eq!(
        receipt.failures,
        [
            RuleWriteFailure::new(FailedRule::Written(0), FailureClass::Permanent),
            RuleWriteFailure::new(FailedRule::Written(1), FailureClass::Permanent),
        ]
    );
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert_eq!(receipt.outcomes[1].kept, Kept::NotKept);
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
    let state = state.lock().unwrap();
    assert_eq!(
        state.writes,
        [
            ("POST", "new".to_owned()),
            ("DELETE", "b".to_owned()),
            ("DELETE", "c".to_owned()),
        ]
    );
    assert_eq!(state.ids(), ["a", "b", "c", "new4"]);
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
    rules[0].body =
        MailRule::typed(Match::all(vec![subject(&["new"])]), vec![Action::Discard]).body;
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

    state
        .lock()
        .unwrap()
        .list
        .push(filter("d", "four@example.com"));
    let err = replace(&client, &no_names(), &base, &base.rules)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert_eq!(state.lock().unwrap().writes.len(), 0);
}

/// Gmail forwards only to an address it has verified: a forward to any other is lost before
/// anything is sent, and the addresses are asked once per write, only where a rule forwards.
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
    let only = MailRule::typed(
        Match::all(vec![from(&["x"])]),
        vec![Action::Forward {
            to: vec![RuleAddress::new("nobody@example.net").unwrap()],
        }],
    );
    rules[2].body = only.body;
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.forwarding_reads, 1);
    // The verified forward is written; the pending one loses its forward and keeps its trash;
    // the third had nothing else to do, so its filter stays as it was.
    assert_eq!(state.ids(), ["c", "new4", "new5"]);
    let forwards: Vec<Option<&str>> = state
        .list
        .iter()
        .map(|f| f["action"]["forward"].as_str())
        .collect();
    assert_eq!(forwards, [None, Some("kept@example.net"), None]);
    assert_eq!(receipt.outcomes[1].kept, Kept::Reduced);
    assert!(receipt.outcomes[1].losses.contains(&Loss::new(
        RulePart::Action(1),
        LossReason::ForwardNotAllowed
    )));
    assert_eq!(receipt.outcomes[2].kept, Kept::NotKept);
    assert!(receipt.outcomes[2].losses.contains(&Loss::new(
        RulePart::Action(0),
        LossReason::ForwardNotAllowed
    )));
    assert_eq!(
        receipt.failures,
        [RuleWriteFailure::new(
            FailedRule::Written(2),
            FailureClass::Permanent
        )]
    );
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
    assert_eq!(state.lock().unwrap().writes.len(), 0);
    assert_eq!(
        receipt.failures,
        [RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::Retryable
        )]
    );
}

#[tokio::test]
async fn a_keyword_is_filed_under_the_label_named_for_it_made_where_missing() {
    let (client, state) = client(Filters::default());
    let base = read(&client).await;
    let work = Keyword::new("$work").unwrap();
    let labels = Labels::new(vec![KeywordName::new(work.clone(), "Work tag").unwrap()]);
    let rule = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![Action::AddKeywords([work].into())],
    );
    let receipt = replace(&client, &labels, &base, &[rule]).await.unwrap();
    assert_eq!(receipt.failures.len(), 0);
    let state = state.lock().unwrap();
    assert_eq!(state.labels[0]["name"], "Work tag");
    assert_eq!(state.list[0]["action"]["addLabelIds"], json!(["Label_900"]));
}
