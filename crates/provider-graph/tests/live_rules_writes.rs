//! Gated live **writes** of the inbox rules and the automatic replies. Not run by default, nor by
//! a token alone: it changes the mailbox, so it needs `GRAPH_RULES_WRITES=1` beside a delegated
//! `GRAPH_ACCESS_TOKEN` with `MailboxSettings.ReadWrite`.
//!
//! What it may touch, and how it leaves the mailbox:
//!
//! - **Only rules it creates.** Every rule it makes holds a marker no other rule holds and sits
//!   after every existing rule; it marks mail read or sets an importance, nothing that sends, moves
//!   or deletes mail. No request of any kind goes to a rule it did not create: before writing it
//!   checks that the existing rules' numbers rise strictly, so the engine has none to renumber, and
//!   it stops otherwise.
//! - **A byte-exact snapshot** of the listing is taken first; afterwards the rules it did not
//!   create must be that snapshot exactly (ids, order, numbers, content), and the count of
//!   differences is all it prints. Whatever happens, the rules holding the marker are removed.
//! - **The reply** is kept exactly as read, written only as a reply scheduled for 2099, and then
//!   `PATCH`ed back verbatim; the setting read afterwards must be the snapshot exactly.
//!
//! It settles what offline fakes cannot: the request shapes, whether a stale `If-Match` on a rule
//! is refused, whether a `PATCH` clears with `null` and `false` (a name included), whether a rule
//! is created without a name, and how Graph keeps `sequence` (gaps, past the rule count, a `PATCH`
//! of the number alone). It prints only statuses, counts and numbers.
//!
//! With `GRAPH_RULES_CAPTURE_DIR` set, it saves what Graph answered for its own rules there, to be
//! scrubbed into fixtures.

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::{Importance, Keyword, SystemKeyword},
    rules::{
        Action, Condition, HeaderField, MailRule, Match, RuleBody, RuleName, TextOp, TextTest,
    },
    time::UtcDateTime,
    vacation::{AutoReply, VacationWindow},
};
use engine_provider::{Provider, VacationPrecondition};
use provider_graph::{GraphClient, GraphProvider};
use serde_json::{Value, json};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const RULES: &str = "/me/mailFolders/inbox/messageRules";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The delegated token, where writes were asked for.
fn token() -> Option<String> {
    let allowed = std::env::var("GRAPH_RULES_WRITES").is_ok_and(|v| v == "1");
    let token = std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    allowed.then_some(token).flatten()
}

fn provider(token: &str) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

/// A marker no other rule holds, and none of this test's runs holds inside another's.
fn marker() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("engine-live-rule-{nanos}-end")
}

async fn send(
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = http()
        .request(method, format!("{GRAPH}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    let answer = request.send().await.expect("request");
    let status = answer.status().as_u16();
    let text = answer.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// The rules as Graph lists them, ordered as the adapter orders them.
async fn listing(token: &str) -> Vec<Value> {
    let (status, page) = send(token, reqwest::Method::GET, RULES, None).await;
    assert_eq!(status, 200, "listing");
    assert!(page.get("@odata.nextLink").is_none(), "one page");
    let mut rules = page["value"].as_array().unwrap().clone();
    rules.sort_by_key(|rule| {
        (
            rule["sequence"].as_i64(),
            rule["id"].as_str().map(str::to_owned),
        )
    });
    rules
}

/// Whether a rule is one of this run's: its name or its subject test holds the marker.
fn ours(rule: &Value, marker: &str) -> bool {
    let named = rule["displayName"]
        .as_str()
        .is_some_and(|n| n.contains(marker));
    let tested = rule["conditions"]["subjectContains"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|v| v.as_str().is_some_and(|v| v.contains(marker)));
    named || tested
}

fn theirs(rules: &[Value], marker: &str) -> Vec<Value> {
    rules.iter().filter(|r| !ours(r, marker)).cloned().collect()
}

/// How many positions differ between two listings, a length difference counting each missing
/// rule: ids, numbers and content alike.
fn differences(before: &[Value], after: &[Value]) -> usize {
    let paired = before.iter().zip(after).filter(|(a, b)| a != b).count();
    paired + before.len().abs_diff(after.len())
}

fn capture(name: &str, value: &Value) {
    if let Ok(dir) = std::env::var("GRAPH_RULES_CAPTURE_DIR") {
        let _ = std::fs::write(format!("{dir}/{name}.json"), value.to_string());
    }
}

fn subject_rule(marker: &str, tag: &str, actions: Vec<Action>) -> MailRule {
    MailRule::typed(
        Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test: TextTest::new(TextOp::Contains, vec![format!("{marker}-{tag}")]).unwrap(),
        }]),
        actions,
    )
    .named(RuleName::new(format!("Engine test {tag} {marker}")).unwrap())
}

fn mark_read() -> Action {
    Action::AddKeywords([Keyword::system(SystemKeyword::Seen)].into())
}

fn subject_holds(rule: &MailRule, marker: &str) -> bool {
    let RuleBody::Typed(typed) = &rule.body else {
        return false;
    };
    typed.when.conditions.iter().any(|c| match c {
        Condition::Header { test, .. } => test.values().iter().any(|v| v.contains(marker)),
        _ => false,
    })
}

/// One path segment, as the adapter writes it.
fn encode(id: &str) -> String {
    id.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test]
async fn live_rules_are_written_and_every_rule_the_test_did_not_create_stays_byte_for_byte() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let marker = marker();
    let before = listing(&token).await;
    // The engine renumbers no rule whose number already rises above the one before it.
    let numbers: Vec<i64> = before
        .iter()
        .filter_map(|r| r["sequence"].as_i64())
        .collect();
    assert_eq!(
        numbers.len(),
        before.len(),
        "unexpected state: a rule without a number"
    );
    assert!(
        numbers.windows(2).all(|pair| pair[0] < pair[1]),
        "unexpected state: the existing rules' numbers do not rise strictly; nothing was written"
    );
    let run = tokio::spawn(rule_cycle(token.clone(), marker.clone(), before.len())).await;
    remove_marked(&token, &marker).await;
    let after = listing(&token).await;
    let changed = differences(&before, &theirs(&after, &marker));
    let left = after.iter().filter(|r| ours(r, &marker)).count();
    eprintln!("differences from the snapshot: {changed}; rules of this run left: {left}");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
    assert_eq!(changed, 0);
    assert_eq!(left, 0);
    assert_eq!(
        Value::Array(before).to_string(),
        Value::Array(after).to_string(),
        "the listing is byte for byte the snapshot"
    );
}

async fn rule_cycle(token: String, marker: String, count: usize) {
    let graph = provider(&token);
    let rules = graph.mail_rules().unwrap();
    let base = rules.read_rules(&account()).await.expect("read");

    // Two rules of ours, after every other rule.
    let a = subject_rule(
        &marker,
        "a",
        vec![mark_read(), Action::MarkImportance(Importance::Low)],
    );
    let b = subject_rule(&marker, "b", vec![mark_read()]);
    let mut written = base.rules.clone();
    written.extend([a.clone(), b.clone()]);
    let receipt = rules
        .replace_rules(&account(), &base, &written)
        .await
        .expect("create");
    assert_eq!(receipt.failures, Vec::new(), "create");
    let created = rules.read_rules(&account()).await.expect("read back");
    assert_eq!(receipt.version, created.version);
    assert_eq!(created.rules.len(), count + 2);
    for (read, sent) in created.rules[count..].iter().zip([&a, &b]) {
        let mut read = read.clone();
        read.origin = None;
        assert_eq!(&read, sent, "read back as written");
    }
    let listed = listing(&token).await;
    let own: Vec<&Value> = listed.iter().filter(|r| ours(r, &marker)).collect();
    capture("created_rule", own[0]);
    let odd_ids = own
        .iter()
        .filter(|r| r["id"].as_str().is_some_and(|id| id.contains(['/', '+'])))
        .count();
    let sequences: Vec<i64> = own.iter().filter_map(|r| r["sequence"].as_i64()).collect();
    eprintln!("created: sequences {sequences:?}; ids with '/' or '+': {odd_ids}");
    let a_id = own[0]["id"].as_str().unwrap().to_owned();
    let b_id = own[1]["id"].as_str().unwrap().to_owned();

    if_match_probe(&token, &a_id, &marker).await;

    // Clearing: A loses its importance and its name.
    let now = rules.read_rules(&account()).await.expect("read");
    let mut cleared = now.rules.clone();
    let a_now = &mut cleared[count];
    if let RuleBody::Typed(typed) = &mut a_now.body {
        typed.actions = vec![mark_read()];
    }
    a_now.name = None;
    let receipt = rules
        .replace_rules(&account(), &now, &cleared)
        .await
        .expect("clear");
    let listed = listing(&token).await;
    let a_raw = listed.iter().find(|r| r["id"] == a_id.as_str()).expect("A");
    capture("cleared_rule", a_raw);
    eprintln!(
        "clearing: failures {}; importance gone: {}; name now: {}",
        receipt.failures.len(),
        a_raw["actions"]
            .get("markImportance")
            .is_none_or(Value::is_null),
        match &a_raw["displayName"] {
            Value::Null => "null",
            Value::String(name) if name.is_empty() => "empty",
            Value::String(_) => "kept",
            _ => "other",
        }
    );

    // Order: B before A, among our rules only.
    let now = rules.read_rules(&account()).await.expect("read");
    let mut swapped = now.rules.clone();
    swapped.swap(count, count + 1);
    let receipt = rules
        .replace_rules(&account(), &now, &swapped)
        .await
        .expect("swap");
    assert_eq!(receipt.failures, Vec::new(), "swap");
    let listed = listing(&token).await;
    let order: Vec<&str> = listed
        .iter()
        .filter(|r| ours(r, &marker))
        .map(|r| if r["id"] == b_id.as_str() { "b" } else { "a" })
        .collect();
    let sequences: Vec<i64> = listed
        .iter()
        .filter(|r| ours(r, &marker))
        .filter_map(|r| r["sequence"].as_i64())
        .collect();
    eprintln!("swapped: order {order:?}, sequences {sequences:?}");
    assert_eq!(order, ["b", "a"]);

    // A number past the rule count, by a PATCH of the number alone.
    let far = i64::try_from(count).unwrap() + 20;
    let path = format!("{RULES}/{}", encode(&b_id));
    let (status, answer) = send(
        &token,
        reqwest::Method::PATCH,
        &path,
        Some(json!({ "sequence": far })),
    )
    .await;
    let kept = listing(&token)
        .await
        .iter()
        .find(|r| r["id"] == b_id.as_str())
        .and_then(|r| r["sequence"].as_i64());
    eprintln!(
        "sequence-only PATCH to {far}: {status}, answered {:?}, listed {kept:?}",
        answer["sequence"].as_i64()
    );

    // A rule created with no name.
    let (status, unnamed) = send(
        &token,
        reqwest::Method::POST,
        RULES,
        Some(json!({
            "sequence": far + 10,
            "isEnabled": true,
            "conditions": { "subjectContains": [format!("{marker}-c")] },
            "actions": { "markAsRead": true }
        })),
    )
    .await;
    capture("created_unnamed_rule", &unnamed);
    eprintln!(
        "POST without displayName: {status}; name answered: {}",
        match &unnamed["displayName"] {
            Value::Null => "null or absent",
            Value::String(name) if name.is_empty() => "empty",
            Value::String(_) => "a name",
            _ => "other",
        }
    );

    // A base read before the last writes is stale.
    let stale = rules
        .replace_rules(&account(), &now, &now.rules)
        .await
        .unwrap_err();
    assert_eq!(stale.class(), FailureClass::Conflict);

    // Ours removed through the engine: everything read but our rules.
    let end = rules.read_rules(&account()).await.expect("read");
    let without: Vec<MailRule> = end
        .rules
        .iter()
        .filter(|r| {
            !r.name
                .as_ref()
                .is_some_and(|n| n.as_str().contains(&marker))
                && !subject_holds(r, &marker)
        })
        .cloned()
        .collect();
    assert_eq!(without.len(), count);
    let receipt = rules
        .replace_rules(&account(), &end, &without)
        .await
        .expect("remove");
    assert_eq!(receipt.failures, Vec::new(), "remove");
}

/// Sends a `PATCH` of our rule's name with a stale `If-Match`, and reports whether Graph refused
/// it. The rule has no ETag to send a fresh one with, so this is the one question to ask.
async fn if_match_probe(token: &str, id: &str, marker: &str) {
    let listed = listing(token).await;
    let mine = listed.iter().find(|r| r["id"] == id).expect("our rule");
    assert!(
        mine.get("@odata.etag").is_none(),
        "a rule now carries an ETag"
    );
    let status = http()
        .patch(format!("{GRAPH}{RULES}/{}", encode(id)))
        .bearer_auth(token)
        .header("If-Match", "W/\"stale-etag\"")
        .header("Content-Type", "application/json")
        .body(json!({ "displayName": format!("Engine test probed {marker}") }).to_string())
        .send()
        .await
        .unwrap()
        .status();
    eprintln!("PATCH of a rule with a stale If-Match: {status}");
    assert!(
        status.is_success() || status.as_u16() == 412,
        "neither landed nor refused as stale: {status}"
    );
}

/// Removes every rule holding the marker, and nothing else.
async fn remove_marked(token: &str, marker: &str) {
    let (status, page) = send(token, reqwest::Method::GET, RULES, None).await;
    if status != 200 {
        return;
    }
    for rule in page["value"].as_array().into_iter().flatten() {
        if let (true, Some(id)) = (ours(rule, marker), rule["id"].as_str()) {
            let path = format!("{RULES}/{}", encode(id));
            let _ = send(token, reqwest::Method::DELETE, &path, None).await;
        }
    }
}

#[tokio::test]
async fn live_a_reply_scheduled_for_2099_is_written_read_back_and_the_setting_restored_exactly() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let path = "/me/mailboxSettings/automaticRepliesSetting";
    let (status, mut kept) = send(&token, reqwest::Method::GET, path, None).await;
    assert_eq!(status, 200);
    kept.as_object_mut().unwrap().remove("@odata.context");
    let graph = provider(&token);
    let before = graph
        .vacation()
        .unwrap()
        .read_vacation(&account())
        .await
        .expect("read");
    let run = tokio::spawn(reply_cycle(token.clone(), before)).await;
    // Put the setting back exactly as it was read, whatever happened.
    let (restored, _) = send(
        &token,
        reqwest::Method::PATCH,
        "/me/mailboxSettings",
        Some(json!({ "automaticRepliesSetting": kept })),
    )
    .await;
    let (_, mut after) = send(&token, reqwest::Method::GET, path, None).await;
    after.as_object_mut().unwrap().remove("@odata.context");
    let changed = kept
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, value)| after.get(key.as_str()) != Some(value))
        .count();
    eprintln!("restore: {restored}; properties differing from the snapshot: {changed}");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
    assert!((200..300).contains(&restored));
    assert_eq!(
        kept.to_string(),
        after.to_string(),
        "the setting is as it was"
    );
}

async fn reply_cycle(token: String, before: engine_core::vacation::VacationResponse) {
    let graph = provider(&token);
    let reply = graph.vacation().unwrap();
    let at = |text: &str| UtcDateTime::parse_rfc3339(text).unwrap();
    let window = VacationWindow::default()
        .with_start(at("2099-01-01T09:00:00Z"))
        .with_end(at("2099-01-02T17:00:00Z"));
    let new = before
        .clone()
        .turned_on()
        .with_window(window)
        .with_reply(AutoReply::default().with_html("<p>Engine live test reply</p>"));
    let receipt = reply
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&before), &new)
        .await
        .expect("write");
    let read = reply.read_vacation(&account()).await.expect("read back");
    assert_eq!(receipt.version, read.version);
    assert!(read.enabled);
    assert_eq!(read.window, window);
    assert!(
        read.reply
            .html
            .as_deref()
            .is_some_and(|html| html.contains("Engine live test reply"))
    );
    let stale = reply
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&before), &new)
        .await
        .unwrap_err();
    assert_eq!(stale.class(), FailureClass::Conflict);
}
