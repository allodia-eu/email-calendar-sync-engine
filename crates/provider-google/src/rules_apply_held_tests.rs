//! The filters' writer where Gmail already holds what a rule asks for, and where the host
//! hands back what it read in another encoding.

use engine_core::{
    mail::MailboxRole,
    rules::{Action, Condition, Kept, MailRule, Match, RuleMailbox, TextOp, TextTest},
};
use serde_json::{Value, json};

use super::{
    replace,
    tests::{discard, no_names, read, three, trash, writes},
};
use crate::{
    rules_fake::{Filters, client},
    rules_read::tests::subject,
};

/// Gmail refuses a filter identical to one it holds (observed): a change the filter does not
/// show keeps the filter and sends nothing.
#[tokio::test]
async fn a_change_the_filter_does_not_show_sends_nothing() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0].body = MailRule::typed(
        Match::all(vec![Condition::Header {
            field: engine_core::rules::HeaderField::Subject,
            test: TextTest::new(TextOp::Contains, vec!["one".to_owned()]).unwrap(),
        }]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Trash),
            copy: false,
        }],
    )
    .body;
    assert_ne!(rules[0], base.rules[0]);
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(writes(&state), Vec::new());
    assert_eq!(receipt.failures, Vec::new());
}

/// A rule a host built that Gmail already holds as a filter takes that filter: nothing is sent,
/// and the filter is not deleted as one no rule names.
#[tokio::test]
async fn a_built_rule_gmail_already_holds_keeps_that_filter() {
    let (client, state) = client(three());
    let base = read(&client).await;
    let mut built = MailRule::typed(Match::all(Vec::new()), Vec::new());
    built.body = trash("two");
    let rules = vec![base.rules[0].clone(), base.rules[2].clone(), built];
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(writes(&state), Vec::new());
    assert_eq!(state.lock().unwrap().ids(), ["a", "b", "c"]);
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(receipt.outcomes[2].kept, Kept::Whole);
}

/// A rule edited into a copy of another filter: where that filter's own rule is left out, the
/// edit takes it, and the filter it was read as goes; where it is still there, the edit is held by
/// it already. Either way no duplicate is sent, and nothing a rule asks for is removed.
#[tokio::test]
async fn a_rule_edited_into_a_copy_of_another_sends_no_duplicate() {
    let (first, state) = client(three());
    let base = read(&first).await;
    let mut alone = vec![base.rules[0].clone(), base.rules[2].clone()];
    alone[0].body = trash("two");
    let receipt = replace(&first, &no_names(), &base, &alone).await.unwrap();
    assert_eq!(writes(&state), [("DELETE", "a".to_owned())]);
    assert_eq!(state.lock().unwrap().ids(), ["b", "c"]);
    assert_eq!(receipt.failures, Vec::new());

    let (gmail, state) = client(three());
    let base = read(&gmail).await;
    let mut both = base.rules.clone();
    both[0].body = trash("two");
    let receipt = replace(&gmail, &no_names(), &base, &both).await.unwrap();
    assert_eq!(writes(&state), [("DELETE", "a".to_owned())]);
    assert_eq!(state.lock().unwrap().ids(), ["b", "c"]);
    assert_eq!(receipt.failures, Vec::new());
}

/// Two rules built alike: the first is created, the second is held by it, never sent twice.
#[tokio::test]
async fn two_built_rules_alike_are_created_once() {
    let (client, state) = client(Filters::default());
    let base = read(&client).await;
    let rule = MailRule::typed(Match::all(vec![subject(&["x"])]), vec![Action::Discard]);
    let receipt = replace(&client, &no_names(), &base, &[rule.clone(), rule])
        .await
        .unwrap();
    assert_eq!(writes(&state), [("POST", "new".to_owned())]);
    assert_eq!(receipt.failures, Vec::new());
}

/// A create answered with a success whose body does not read was done: the status says so, and
/// the filter it replaces goes as after any create.
#[tokio::test]
async fn a_create_whose_answer_does_not_read_was_done() {
    let mut filters = three();
    filters.unreadable_create = true;
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0].body = discard("new");
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(state.lock().unwrap().ids(), ["b", "c", "new4"]);
}

/// The raw form as a host might hand it back after storing it: each object's keys in reverse
/// order, and spaces and line breaks between them.
pub(crate) fn re_encoded(raw: &str) -> String {
    let Value::Array(items) = serde_json::from_str(raw).unwrap() else {
        panic!("a listing");
    };
    let items: Vec<String> = items
        .iter()
        .map(|item| {
            let fields: Vec<String> = item
                .as_object()
                .unwrap()
                .iter()
                .rev()
                .map(|(key, value)| format!("\n  {} : {}", json!(key), value))
                .collect();
            format!("{{{} }}", fields.join(" ,"))
        })
        .collect();
    format!("[ {} ]\n", items.join(", "))
}

/// A base whose raw form the host re-encoded is still the listing read: the write goes ahead.
#[tokio::test]
async fn a_base_re_encoded_by_the_host_is_accepted() {
    let (client, state) = client(three());
    let mut base = read(&client).await;
    let raw = base.raw.as_ref().unwrap().as_str().to_owned();
    let again = re_encoded(&raw);
    assert_ne!(again, raw);
    base.raw = Some(engine_core::raw::RawRules::new(again));
    let mut rules = base.rules.clone();
    rules[1].body = discard("new");
    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(state.lock().unwrap().ids(), ["a", "c", "new4"]);
}

/// A filter not written because no credential could be had says so in the receipt, beside the
/// source's class; one the server refused does not.
#[test]
fn a_filter_written_without_a_credential_says_so_in_the_receipt() {
    use crate::rules_apply_support::failure;

    let no_credential = crate::GoogleError::from(
        engine_provider::CredentialFailure::refused_origin("refused"),
    );
    let failed = failure(3, &no_credential);
    assert_eq!(failed.class, engine_core::error::FailureClass::Permanent);
    assert!(failed.credential_failure, "{failed:?}");
    assert!(!failed.outcome_unknown);

    let refused = failure(3, &crate::GoogleError::status(401, "{}"));
    assert!(!refused.credential_failure, "{refused:?}");
}

/// A throttle reaches every later rule, as it reaches every later message of an import: the
/// write sends nothing more, and each rule it would have sent is in the receipt with the
/// throttle's class.
#[tokio::test]
async fn a_throttle_ends_the_write_and_the_rules_after_it_are_not_sent() {
    use engine_core::{
        error::FailureClass,
        rules::{FailedRule, RuleWriteFailure},
    };

    let mut filters = three();
    filters.failing.insert(("POST", "new".to_owned()), 429);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut rules = base.rules.clone();
    rules[0].body = discard("new one");
    rules[1].body = discard("new two");

    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();

    assert_eq!(writes(&state), [("POST", "new".to_owned())]);
    assert_eq!(
        receipt.failures,
        vec![
            RuleWriteFailure::new(FailedRule::Written(0), FailureClass::RateLimited),
            RuleWriteFailure::new(FailedRule::Written(1), FailureClass::RateLimited),
        ]
    );
    assert_eq!(state.lock().unwrap().ids(), ["a", "b", "c"]);
}

/// After a throttle ends the write, a later rule whose filter Gmail already holds needs no
/// request: it takes that filter, is not listed as unwritten, and the filter is not deleted as
/// one no rule names. Only a request is refused after an ending, never a rule.
#[tokio::test]
async fn after_an_ending_a_rule_gmail_already_holds_keeps_its_filter() {
    use engine_core::{
        error::FailureClass,
        rules::{FailedRule, RuleWriteFailure},
    };

    let mut filters = three();
    filters.failing.insert(("POST", "new".to_owned()), 429);
    let (client, state) = client(filters);
    let base = read(&client).await;
    let mut changed = base.rules[0].clone();
    changed.body = discard("new one");
    let mut built = MailRule::typed(Match::all(Vec::new()), Vec::new());
    built.body = trash("two");
    let rules = vec![changed, base.rules[2].clone(), built];

    let receipt = replace(&client, &no_names(), &base, &rules).await.unwrap();

    assert_eq!(writes(&state), [("POST", "new".to_owned())]);
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::RateLimited
        )]
    );
    assert_eq!(state.lock().unwrap().ids(), ["a", "b", "c"]);
    assert_eq!(receipt.outcomes[2].kept, Kept::Whole);
    assert_eq!(receipt.left_as_read, [0]);
}
