//! Where the writer puts each rule: Graph keeps the rules as a list whose `sequence` is a
//! position, renumbered on every change (observed), so only what moves the list is sent.

use engine_core::{
    error::FailureClass,
    rules::{FailedRule, Kept, Loss, LossReason, RulePart, RuleWriteFailure},
};
use serde_json::json;

use super::{
    replace,
    tests::{base, client, new_rule, writes},
};
use crate::{
    rules_read::tests::{CAPTURED, SHAPES},
    test_support::json,
};

#[tokio::test]
async fn a_rule_put_first_is_posted_first_and_no_other_rule_is_sent() {
    // Graph inserts at the position asked and moves the rest down itself (observed).
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = vec![new_rule()];
    rules.extend(base.rules.iter().cloned());
    let _ = replace(&client, &base, &rules).await.unwrap();
    let sent = writes(&log);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "POST");
    assert_eq!(sent[0].2.as_ref().unwrap()["sequence"], 1);
}

#[tokio::test]
async fn moving_rules_sends_only_the_moves_graph_needs() {
    // From 1 2 3 4 5 to 2 1 5 3 4: moving the second to the top puts the first second, and
    // moving the fifth to third puts the third and fourth after it.
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = vec![
        base.rules[1].clone(),
        base.rules[0].clone(),
        base.rules[4].clone(),
        base.rules[2].clone(),
        base.rules[3].clone(),
    ];
    rules.extend(base.rules[5..].iter().cloned());
    let _ = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(
        writes(&log),
        vec![
            (
                "PATCH",
                "messageRules/AQAAAAAAAAI%3D".to_owned(),
                Some(json!({"sequence": 1}))
            ),
            (
                "PATCH",
                "messageRules/AQAAAAAAAAU%3D".to_owned(),
                Some(json!({"sequence": 3}))
            ),
        ]
    );
}

#[tokio::test]
async fn a_read_only_rule_still_in_order_is_in_its_place_and_one_out_of_order_is_reported() {
    let (client, log) = client(SHAPES, vec![]);
    let base = base(&client).await;
    // The first rule goes: the read-only second is still in order, so nothing is wrong.
    let receipt = replace(&client, &base, &base.rules[1..]).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(writes(&log).len(), 1, "the DELETE alone");
    log.lock().unwrap().clear();
    // The third first: the read-only one cannot move after it.
    let rules = vec![
        base.rules[2].clone(),
        base.rules[1].clone(),
        base.rules[0].clone(),
    ];
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(1),
            FailureClass::Permanent
        )]
    );
    assert_eq!(receipt.outcomes[1].kept, Kept::Reduced);
    assert_eq!(
        receipt.outcomes[1].losses,
        vec![
            // It is the one the fixture marks in error, too.
            Loss::new(RulePart::Rule, LossReason::SourceInError),
            Loss::new(RulePart::Order, LossReason::NotWritten),
        ]
    );
    // The third moves to the top; the read-only one is never sent, and the first is then
    // already next.
    assert_eq!(
        writes(&log),
        vec![(
            "PATCH",
            "messageRules/AQAAAAAAAQM%3D".to_owned(),
            Some(json!({"sequence": 1}))
        )]
    );
}

#[tokio::test]
async fn a_rule_left_out_is_deleted_and_the_rest_keep_their_numbers() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    rules.remove(2);
    let receipt = replace(&client, &base, &rules).await.unwrap();
    // A gap in the numbers is still the order: the rules after it are not sent.
    assert_eq!(
        writes(&log),
        vec![("DELETE", "messageRules/AQAAAAAAAAM%3D".to_owned(), None)]
    );
    assert!(receipt.outcomes.iter().all(|o| o.kept == Kept::Whole));
}

#[tokio::test]
async fn a_rule_id_is_one_percent_encoded_path_segment() {
    let mut listing = json(CAPTURED);
    listing["value"][0]["id"] = json!("AQ+AAAAAAAE=");
    let fixture = listing.to_string();
    let (client, log) = client(&fixture, vec![]);
    let base = base(&client).await;
    let receipt = replace(&client, &base, &base.rules[1..]).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(
        writes(&log),
        vec![("DELETE", "messageRules/AQ%2BAAAAAAAE%3D".to_owned(), None)]
    );
}

/// A rule not written because no credential could be had says so in the receipt, beside the
/// source's class; one the server refused does not.
#[test]
fn a_rule_written_without_a_credential_says_so_in_the_receipt() {
    let no_credential = crate::GraphError::from(
        engine_provider::CredentialFailure::refused_origin("refused"),
    );
    let failed = super::failure(3, &no_credential);
    assert_eq!(failed.class, FailureClass::Permanent);
    assert!(failed.credential_failure, "{failed:?}");
    assert!(!failed.outcome_unknown);

    let refused = super::failure(3, &crate::GraphError::status(401, "{}"));
    assert_eq!(refused.class, FailureClass::Authentication);
    assert!(!refused.credential_failure, "{refused:?}");
}
