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

pub(super) fn envelope(code: &str) -> Value {
    json!({"error": {"code": code, "message": "server words"}})
}

/// A client whose listing is `fixture`, with `extra` routes matched first.
pub(super) fn client(fixture: &str, extra: Vec<(&str, FakeRoute)>) -> (GraphClient, RequestLog) {
    let mut routes = extra;
    routes.push(("mailFolders/junkemail", Ok(json!({"id": "junk-id"}))));
    routes.push(("messageRules", Ok(json(fixture))));
    let (client, _, log) = fake_client_logged(routes);
    (client, log)
}

pub(super) async fn base(client: &GraphClient) -> RuleSet {
    Listing::read(client).await.unwrap().rule_set()
}

/// The writes sent, as `(method, url tail, body)`.
pub(super) fn writes(log: &RequestLog) -> Vec<(&'static str, String, Option<Value>)> {
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

pub(super) fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap(),
    }
}

pub(super) fn new_rule() -> MailRule {
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
    // The listing can lag the writes: the receipt names no version to trust.
    assert_eq!(receipt.version, None);
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
async fn a_changed_rule_dropping_nothing_is_patched_in_place() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
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
    assert_eq!(body["actions"]["stopProcessingRules"], true);
    assert!(body.get("sequence").is_none(), "its place did not move");
}

#[tokio::test]
async fn a_changed_rule_dropping_a_part_is_created_in_its_place_and_the_old_one_removed() {
    // Graph keeps on a PATCH what it is asked to clear (observed), so the rule is written anew.
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    if let engine_core::rules::RuleBody::Typed(typed) = &mut rules[0].body {
        typed
            .actions
            .retain(|a| matches!(a, Action::FileInto { .. }));
    }
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    let sent = writes(&log);
    assert_eq!(sent.len(), 2);
    let (method, url, body) = &sent[0];
    assert_eq!((*method, url.as_str()), ("POST", "messageRules"));
    let body = body.as_ref().unwrap();
    assert_eq!(body["sequence"], 1);
    assert_eq!(body["displayName"], "Example rule 1");
    assert!(body["actions"].get("forwardTo").is_none());
    assert!(body["actions"].get("markAsRead").is_none());
    assert_eq!(
        (sent[1].0, sent[1].1.as_str()),
        ("DELETE", "messageRules/AQAAAAAAAAE%3D")
    );
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
    // Removals go first, so the positions are those of the rules that stay.
    assert_eq!(
        receipt.failures,
        vec![
            RuleWriteFailure::new(FailedRule::Removed(2), FailureClass::Retryable)
                .outcome_unknown(),
            RuleWriteFailure::new(FailedRule::Written(1), FailureClass::Permanent),
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

/// The edit is not kept, so it does not apply: the rule as read stays where it is, nothing is
/// sent for it, and the receipt names it (proven offline only: Graph writes are not run live for
/// this).
#[tokio::test]
async fn a_rule_changed_into_one_graph_cannot_hold_leaves_the_rule_as_read() {
    let (client, log) = client(CAPTURED, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    if let engine_core::rules::RuleBody::Typed(typed) = &mut rules[8].body {
        typed.when.conditions = vec![subject("a"), subject("b")];
    }
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(receipt.outcomes[8].kept, Kept::NotKept);
    assert_eq!(writes(&log).len(), 0);
    assert_eq!(receipt.left_as_read, [8]);
    assert_eq!(receipt.failures, Vec::new());
}

/// A rule whose write failed leaves the rule it was read as too, and the receipt says so beside
/// the failure.
#[tokio::test]
async fn a_rule_whose_write_failed_is_left_as_read() {
    let (client, _log) = client(SHAPES, vec![]);
    let base = base(&client).await;
    let mut rules = base.rules.clone();
    // Read-only rules are never sent: a change to one is refused, and it stays.
    let index = base
        .rules
        .iter()
        .position(|rule| rule.read_only)
        .expect("the shapes hold a read-only rule");
    rules[index].name = Some(RuleName::new("Renamed").unwrap());
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(receipt.outcomes[index].kept, Kept::NotKept);
    assert_eq!(receipt.left_as_read, [index]);
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
