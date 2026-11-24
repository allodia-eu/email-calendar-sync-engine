//! Gated live **writes** of the inbox rules and the automatic replies. Not run by default, nor by
//! a token alone: it changes the mailbox, so it needs `GRAPH_RULES_WRITES=1` beside a delegated
//! `GRAPH_ACCESS_TOKEN` with `MailboxSettings.ReadWrite`.
//!
//! What it may touch, and how it leaves the mailbox:
//!
//! - **Only rules it creates.** Every rule it makes holds a marker no other rule holds and sits
//!   after every existing rule; it marks mail read or sets an importance, nothing that sends, moves
//!   or deletes mail. No request of any kind goes to a rule it did not create: before writing it
//!   checks that the existing rules' numbers rise strictly from 1, so the engine has none to move,
//!   and it stops otherwise.
//! - **One write at a time, each settled.** Graph answers a write before it applies it, a read
//!   straight after can show a state a later read does not, and two quick `mailboxSettings` writes
//!   have landed in the wrong order (observed). So after every write the test reads until the
//!   expected state holds for several reads over a settle period ([`settled`]) before it writes
//!   again, and it never writes and restores in quick succession.
//! - **A byte-exact snapshot** of the listing is taken first; afterwards, settled, the rules it did
//!   not create must be that snapshot exactly (ids, order, numbers, content), and the count of
//!   differences is all it prints. Whatever happens, the rules holding the marker are removed.
//! - **The reply** is kept exactly as read, written once as a reply scheduled for 2099, and only
//!   once that has settled `PATCH`ed back verbatim; the setting must then settle on the snapshot.
//!
//! Settled by its first run (`graph.md`): a stale `If-Match` on a rule is ignored, `sequence` is a
//! dense position, clamped to the end, a `PATCH` of `sequence` alone works, a `POST` without a
//! name is accepted and named by Graph, an id with `=` goes as `%3D`, and `null` clears nothing.
//! It prints only statuses, counts and numbers.

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

/// Reads with `read` until `holds` is true of three reads in a row, ten seconds apart, within
/// five minutes: Graph applies a write some time after answering it.
async fn settled<T, F, R>(what: &str, read: F, holds: impl Fn(&T) -> bool) -> T
where
    F: Fn() -> R,
    R: std::future::Future<Output = T>,
{
    let mut streak = 0;
    for _ in 0..30 {
        let now = read().await;
        streak = if holds(&now) { streak + 1 } else { 0 };
        if streak == 3 {
            return now;
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    panic!("{what} did not settle within five minutes");
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

/// The subject tag of one of this run's rules (`a`, `b`), from its subject test.
fn tag(rule: &Value, marker: &str) -> Option<String> {
    rule["conditions"]["subjectContains"]
        .as_array()?
        .iter()
        .find_map(|v| {
            v.as_str()?
                .strip_prefix(&format!("{marker}-"))
                .map(str::to_owned)
        })
}

fn tags(rules: &[Value], marker: &str) -> Vec<String> {
    rules.iter().filter_map(|r| tag(r, marker)).collect()
}

#[tokio::test]
async fn live_rules_are_written_and_every_rule_the_test_did_not_create_stays_byte_for_byte() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let marker = marker();
    let before = listing(&token).await;
    // Graph numbers the rules 1 to n; the engine moves none of them when ours go after.
    let numbers: Vec<i64> = before
        .iter()
        .filter_map(|r| r["sequence"].as_i64())
        .collect();
    let dense: Vec<i64> = (1..=i64::try_from(before.len()).unwrap()).collect();
    assert_eq!(
        numbers, dense,
        "unexpected state: the rules are not numbered 1 to n; nothing was written"
    );
    let run = tokio::spawn(rule_cycle(token.clone(), marker.clone(), before.len())).await;
    remove_marked(&token, &marker).await;
    let after = settled(
        "the listing after cleanup",
        || listing(&token),
        |rules: &Vec<Value>| !rules.iter().any(|r| ours(r, &marker)),
    )
    .await;
    let changed = differences(&before, &theirs(&after, &marker));
    eprintln!("differences from the snapshot: {changed}");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
    assert_eq!(changed, 0);
    assert_eq!(
        Value::Array(before).to_string(),
        Value::Array(after).to_string(),
        "the listing is byte for byte the snapshot"
    );
}

async fn rule_cycle(token: String, marker: String, count: usize) {
    let graph = provider(&token);
    let rules = graph.mail_rules().unwrap();
    let ours_in = |order: &'static [&'static str]| {
        let marker = marker.clone();
        move |listed: &Vec<Value>| tags(listed, &marker) == order
    };

    // Two rules of ours, after every other rule.
    let base = rules.read_rules(&account()).await.expect("read");
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
    let listed = settled(
        "the created rules",
        || listing(&token),
        ours_in(&["a", "b"]),
    )
    .await;
    let numbers: Vec<i64> = listed
        .iter()
        .filter(|r| ours(r, &marker))
        .filter_map(|r| r["sequence"].as_i64())
        .collect();
    eprintln!("created: sequences {numbers:?}");
    let created = rules.read_rules(&account()).await.expect("read back");
    for (read, sent) in created.rules[count..].iter().zip([&a, &b]) {
        let mut read = read.clone();
        read.origin = None;
        assert_eq!(&read, sent, "read back as written");
    }
    capture(
        "created_rule",
        listed.iter().find(|r| ours(r, &marker)).unwrap(),
    );

    // A drops its importance: a new rule in its place, the old one removed.
    let mut dropped = created.rules.clone();
    if let RuleBody::Typed(typed) = &mut dropped[count].body {
        typed.actions = vec![mark_read()];
    }
    let receipt = rules
        .replace_rules(&account(), &created, &dropped)
        .await
        .expect("drop");
    assert_eq!(receipt.failures, Vec::new(), "drop");
    let listed = settled(
        "the rule written anew",
        || listing(&token),
        |listed: &Vec<Value>| {
            tags(listed, &marker) == ["a", "b"]
                && listed.iter().any(|r| {
                    tag(r, &marker).as_deref() == Some("a")
                        && r["actions"]
                            .get("markImportance")
                            .is_none_or(Value::is_null)
                })
        },
    )
    .await;
    capture(
        "rule_written_anew",
        listed.iter().find(|r| ours(r, &marker)).unwrap(),
    );

    // B before A, among our rules only.
    let before_swap = rules.read_rules(&account()).await.expect("read");
    let mut swapped = before_swap.rules.clone();
    swapped.swap(count, count + 1);
    let receipt = rules
        .replace_rules(&account(), &before_swap, &swapped)
        .await
        .expect("swap");
    assert_eq!(receipt.failures, Vec::new(), "swap");
    let listed = settled("the swap", || listing(&token), ours_in(&["b", "a"])).await;
    let numbers: Vec<i64> = listed
        .iter()
        .filter(|r| ours(r, &marker))
        .filter_map(|r| r["sequence"].as_i64())
        .collect();
    eprintln!("swapped: sequences {numbers:?}");

    // Read before the swap, the base is stale now that the swap has settled.
    let stale = rules
        .replace_rules(&account(), &before_swap, &before_swap.rules)
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
    settled("the removal", || listing(&token), ours_in(&[])).await;
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

const SETTING: &str = "/me/mailboxSettings/automaticRepliesSetting";

/// The reply's setting as Graph answers it, without the context that names the mailbox.
async fn setting(token: &str) -> Value {
    let (status, mut setting) = send(token, reqwest::Method::GET, SETTING, None).await;
    assert_eq!(status, 200);
    setting.as_object_mut().unwrap().remove("@odata.context");
    setting
}

#[tokio::test]
async fn live_a_reply_scheduled_for_2099_is_written_settled_and_the_setting_restored_exactly() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let kept = setting(&token).await;
    let graph = provider(&token);
    let before = graph
        .vacation()
        .unwrap()
        .read_vacation(&account())
        .await
        .expect("read");
    let run = tokio::spawn(reply_cycle(token.clone(), before)).await;
    // Put the setting back exactly as it was read, once the write above has settled (or failed),
    // and wait until the restore has settled in turn.
    let (restored, _) = send(
        &token,
        reqwest::Method::PATCH,
        "/me/mailboxSettings",
        Some(json!({ "automaticRepliesSetting": kept })),
    )
    .await;
    let after = settled(
        "the restored setting",
        || setting(&token),
        |now: &Value| *now == kept,
    )
    .await;
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
    // Graph settles the write later: the receipt names no version.
    assert_eq!(receipt.version, None);
    let read = settled(
        "the written reply",
        || async { reply.read_vacation(&account()).await.expect("read back") },
        |read: &engine_core::vacation::VacationResponse| read.enabled && read.window == window,
    )
    .await;
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
