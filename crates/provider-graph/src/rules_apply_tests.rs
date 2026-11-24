//! Replacing the rules over a fake Graph that logs each request: what is sent for each kind of
//! edit, what is refused before sending, and each per-rule failure landing in the receipt.

use engine_core::{
    error::FailureClass,
    mail::MailboxRole,
    raw::RawRules,
    rules::{
        Action, Condition, FailedRule, HeaderField, Kept, Loss, LossReason, MailRule, Match,
        RuleMailbox, RuleName, RulePart, RuleSet, RuleWriteFailure, TextOp, TextTest,
    },
    version::RuleSetVersion,
};
use serde_json::{Value, json};

use super::replace;
use crate::{
    rules::Listing,
    rules_read::tests::{CAPTURED, SHAPES},
    test_support::{FakeRoute, RequestLog, fake_client_logged, json},
    transport::GraphClient,
};

fn envelope(code: &str) -> Value {
    json!({"error": {"code": code, "message": "server words"}})
}

/// A client whose listing is `fixture`, with `extra` routes matched first.
fn client(fixture: &str, extra: Vec<(&str, FakeRoute)>) -> (GraphClient, RequestLog) {
    let mut routes = extra;
    routes.push(("mailFolders/junkemail", Ok(json!({"id": "junk-id"}))));
    routes.push(("messageRules", Ok(json(fixture))));
    let (client, _, log) = fake_client_logged(routes);
    (client, log)
}

async fn base(client: &GraphClient) -> RuleSet {
    Listing::read(client).await.unwrap().rule_set()
}

/// The writes sent, as `(method, url tail, body)`.
fn writes(log: &RequestLog) -> Vec<(&'static str, String, Option<Value>)> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(method, ..)| *method != "GET")
        .map(|(method, url, body)| {
            let tail = url.rsplit("/inbox/").next().unwrap().to_owned();
            (*method, tail, body.clone())
        })
        .collect()
}

fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap(),
    }
}

fn new_rule() -> MailRule {
    MailRule::typed(Match::all(vec![subject("new")]), vec![Action::Discard])
        .named(RuleName::new("New").unwrap())
}

#[tokio::test]
async fn the_rules_as_read_are_written_back_with_no_request_but_the_reads() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let receipt = replace(&client, &base, &base.rules).await.unwrap();
    assert_eq!(writes(&log).len(), 0);
    assert_eq!(receipt.failures.len(), 0);
    assert_eq!(receipt.version, base.version);
    assert!(receipt.outcomes.iter().all(|o| o.kept == Kept::Whole));
}

#[tokio::test]
async fn a_base_that_was_not_read_is_refused_before_anything_is_sent() {
    let (client, log) = client(CAPTURED, vec![]);
    let read = base(&client).await;
    log.lock().unwrap().clear();
    let mut unversioned = read.clone();
    unversioned.version = None;
    let mut unraw = read.clone();
    unraw.raw = None;
    let mismatched = read.clone().with_raw(RawRules::new("[]"));
    for built in [RuleSet::new(Vec::new()), unversioned, unraw, mismatched] {
        let err = replace(&client, &built, &read.rules).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
    }
    assert_eq!(log.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn rules_that_moved_since_the_read_are_a_conflict_and_nothing_is_written() {
    let (other, _) = client(SHAPES, vec![]);
    let stale = base(&other).await;
    let (client, log) = client(CAPTURED, vec![]);
    let err = replace(&client, &stale, &stale.rules).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert_eq!(writes(&log).len(), 0);
    // A version that names no listing is one too.
    let forged = stale.with_version(RuleSetVersion::new("0"));
    assert_eq!(
        replace(&client, &forged, &[]).await.unwrap_err().class(),
        FailureClass::Permanent
    );
}

#[tokio::test]
async fn a_rule_added_last_is_posted_in_its_place() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    rules.push(new_rule());
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(receipt.failures.len(), 0);
    let sent = writes(&log);
    assert_eq!(sent.len(), 1);
    let (method, url, body) = &sent[0];
    assert_eq!((*method, url.as_str()), ("POST", "messageRules"));
    assert_eq!(
        body.as_ref().unwrap(),
        &json!({
            "displayName": "New",
            "isEnabled": true,
            "sequence": 10,
            "conditions": {"subjectContains": ["new"]},
            "exceptions": {},
            "actions": {"permanentDelete": true}
        })
    );
}

#[tokio::test]
async fn a_changed_rule_is_patched_whole_with_what_it_dropped_cleared() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    // The first rule loses its forward and its read mark, and gets a new name.
    if let engine_core::rules::RuleBody::Typed(typed) = &mut rules[0].body {
        typed
            .actions
            .retain(|a| matches!(a, Action::FileInto { .. }));
    }
    rules[0].name = Some(RuleName::new("Renamed").unwrap());
    let _ = replace(&client, &base, &rules).await.unwrap();
    let sent = writes(&log);
    assert_eq!(sent.len(), 1);
    let (method, url, body) = &sent[0];
    assert_eq!(
        (*method, url.as_str()),
        ("PATCH", "messageRules/AQAAAAAAAAE%3D")
    );
    let body = body.as_ref().unwrap();
    assert_eq!(body["displayName"], "Renamed");
    assert_eq!(body["actions"]["forwardTo"], Value::Null);
    assert_eq!(body["actions"]["markAsRead"], false);
    assert_eq!(body["actions"]["stopProcessingRules"], true);
    assert!(body.get("sequence").is_none(), "its place did not move");
}

#[tokio::test]
async fn a_rule_put_first_is_posted_first_and_every_rule_after_it_placed_again() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = vec![new_rule()];
    rules.extend(base.rules.iter().cloned());
    let _ = replace(&client, &base, &rules).await.unwrap();
    let sent = writes(&log);
    assert_eq!(sent.len(), 10);
    assert_eq!(sent[0].0, "POST");
    assert_eq!(sent[0].2.as_ref().unwrap()["sequence"], 1);
    for (place, (method, _, body)) in sent.iter().enumerate().skip(1) {
        assert_eq!(*method, "PATCH");
        assert_eq!(body.as_ref().unwrap(), &json!({"sequence": place + 1}));
    }
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
async fn each_failed_request_lands_in_the_receipt_and_the_rest_go_ahead() {
    let (client, log) = client(
        CAPTURED,
        vec![
            (
                "messageRules/AQAAAAAAAAI%3D",
                Err((400, envelope("ErrorInvalidRequest"))),
            ),
            (
                "messageRules/AQAAAAAAAAM%3D",
                Err((504, envelope("GatewayTimeout"))),
            ),
            (
                "messageRules/AQAAAAAAAAQ%3D",
                Err((404, envelope("ErrorItemNotFound"))),
            ),
        ],
    );
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    rules[1].name = Some(RuleName::new("Renamed").unwrap());
    rules[4].name = Some(RuleName::new("Renamed too").unwrap());
    // Rules 3 and 4 (indices 2 and 3) are left out: one times out, one is already gone.
    let rules: Vec<MailRule> = rules
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != 2 && *i != 3)
        .map(|(_, r)| r)
        .collect();
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![
            RuleWriteFailure::new(FailedRule::Written(1), FailureClass::Permanent),
            RuleWriteFailure::new(FailedRule::Removed(2), FailureClass::Retryable)
                .outcome_unknown(),
        ]
    );
    // The rule the server refused is not kept as given, and its outcome says so.
    assert_eq!(receipt.outcomes[1].kept, Kept::NotKept);
    assert_eq!(
        receipt.outcomes[1].losses,
        vec![Loss::new(RulePart::Rule, LossReason::NotWritten)]
    );
    // The fifth rule's rename went out all the same, its number kept.
    let sent = writes(&log);
    assert!(sent.iter().any(|(m, url, body)| *m == "PATCH"
        && url == "messageRules/AQAAAAAAAAU%3D"
        && body.as_ref().unwrap()["displayName"] == "Renamed too"
        && body.as_ref().unwrap().get("sequence").is_none()));
    // The failure carries no server words.
    assert!(!format!("{receipt:?}").contains("server words"));
}

#[tokio::test]
async fn a_read_only_rule_is_never_sent_and_a_change_to_it_is_reported() {
    let (client, log) = client(SHAPES, vec![]);
    let base = base(&client).await;
    assert!(base.rules[1].read_only);
    let mut changed = base.rules.clone();
    changed[1].name = Some(RuleName::new("Changed").unwrap());
    let receipt = replace(&client, &base, &changed).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(1),
            FailureClass::Permanent
        )]
    );
    assert_eq!(writes(&log).len(), 0);
    let mut removed = base.rules.clone();
    removed.remove(1);
    let receipt = replace(&client, &base, &removed).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Removed(1),
            FailureClass::Permanent
        )]
    );
    // The read-only one was not deleted, and the third keeps its number.
    assert_eq!(writes(&log).len(), 0);
    assert_eq!(receipt.outcomes[0].kept, Kept::Whole);
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
    // The first follows the third, the read-only one never sent.
    assert_eq!(
        writes(&log),
        vec![(
            "PATCH",
            "messageRules/AQAAAAAAAQE%3D".to_owned(),
            Some(json!({"sequence": 4}))
        )]
    );
}

#[tokio::test]
async fn a_rule_graph_cannot_hold_is_not_kept_and_says_which_part() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    rules.push(MailRule::typed(
        Match::all(vec![subject("a"), subject("b")]),
        vec![Action::Discard],
    ));
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(writes(&log).len(), 0);
    assert_eq!(receipt.outcomes[9].kept, Kept::NotKept);
    assert_eq!(
        receipt.outcomes[9].losses,
        vec![Loss::new(
            RulePart::Condition(1),
            LossReason::NotExpressible
        )]
    );
}

#[tokio::test]
async fn a_role_is_filed_into_by_the_folder_graph_names_for_it() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    let junk = Action::FileInto {
        mailbox: RuleMailbox::Role(MailboxRole::Junk),
        copy: false,
    };
    rules.push(MailRule::typed(
        Match::all(vec![subject("j")]),
        vec![junk.clone()],
    ));
    rules.push(MailRule::typed(Match::all(vec![subject("k")]), vec![junk]));
    let _ = replace(&client, &base, &rules).await.unwrap();
    let lookups = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, url, _)| url.contains("junkemail"))
        .count();
    assert_eq!(lookups, 1, "looked up once");
    let sent = writes(&log);
    assert_eq!(
        sent[0].2.as_ref().unwrap()["actions"]["moveToFolder"],
        "junk-id"
    );
}

#[tokio::test]
async fn a_folder_lookup_that_fails_is_that_rules_failure() {
    let (client, _) = client(
        CAPTURED,
        vec![(
            "mailFolders/archive",
            Err((503, envelope("ServiceUnavailable"))),
        )],
    );
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    rules.push(MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Archive),
            copy: false,
        }],
    ));
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(9),
            FailureClass::Retryable
        )]
    );
    assert_eq!(receipt.outcomes[9].kept, Kept::NotKept);
}

#[tokio::test]
async fn an_existing_rule_the_writer_cannot_place_stays_as_it_was() {
    // The account has no Archive: the plan cannot know, so the writer cannot place the move.
    let (client, log) = client(
        CAPTURED,
        vec![(
            "mailFolders/archive",
            Err((404, envelope("ErrorItemNotFound"))),
        )],
    );
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    if let engine_core::rules::RuleBody::Typed(typed) = &mut rules[3].body {
        typed.actions = vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Archive),
            copy: false,
        }];
    }
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(3),
            FailureClass::Permanent
        )]
    );
    assert_eq!(receipt.outcomes[3].kept, Kept::NotKept);
    // Neither changed nor removed.
    assert_eq!(writes(&log).len(), 0);
}

#[tokio::test]
async fn a_rule_id_is_one_percent_encoded_path_segment() {
    let mut listing = json(CAPTURED);
    listing["value"][0]["id"] = json!("AQ/A+AAAAAE=");
    let fixture = listing.to_string();
    let (client, log) = client(&fixture, vec![]);
    let base = base(&client).await;
    let receipt = replace(&client, &base, &base.rules[1..]).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(
        writes(&log),
        vec![("DELETE", "messageRules/AQ%2FA%2BAAAAAE%3D".to_owned(), None)]
    );
}

#[tokio::test]
async fn a_rule_whose_number_still_fits_is_placed_again_after_one_before_it_moved() {
    // From 1 2 3 4 5 to 2 1 5 3 4. The fifth's number is above the first's new one, but where
    // Exchange renumbers the others on each change, the numbers read are stale once the first has
    // moved: every rule after a move takes the next number.
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
    let placed: Vec<(String, Value)> = writes(&log)
        .into_iter()
        .map(|(_, url, body)| (url, body.unwrap()["sequence"].clone()))
        .collect();
    // The second keeps 2; every rule after the first's move rises from 3.
    assert_eq!(
        placed[0],
        ("messageRules/AQAAAAAAAAE%3D".to_owned(), json!(3))
    );
    assert_eq!(
        placed[1],
        ("messageRules/AQAAAAAAAAU%3D".to_owned(), json!(4))
    );
    let numbers: Vec<u64> = placed.iter().map(|(_, n)| n.as_u64().unwrap()).collect();
    assert_eq!(numbers, (3..=10).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_rule_changed_into_one_graph_cannot_hold_is_not_kept_and_goes() {
    // As on every transport, the set written is the set planned: a rule not kept is not there.
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    if let engine_core::rules::RuleBody::Typed(typed) = &mut rules[8].body {
        typed.when.conditions = vec![subject("a"), subject("b")];
    }
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(receipt.outcomes[8].kept, Kept::NotKept);
    let sent = writes(&log);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].0, sent[0].1.as_str()),
        ("DELETE", "messageRules/AQAAAAAAAAk%3D")
    );
}

#[tokio::test]
async fn a_patch_keeps_the_names_of_the_recipients_it_sends() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    rules[8].name = Some(RuleName::new("Renamed").unwrap());
    let _ = replace(&client, &base, &rules).await.unwrap();
    let sent = writes(&log);
    let forward = &sent[0].2.as_ref().unwrap()["actions"]["forwardTo"][0]["emailAddress"];
    assert_eq!(forward["name"], "Person 5");
}
