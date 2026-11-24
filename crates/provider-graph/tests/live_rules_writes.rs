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
//! - **The reply** is written only where it is off (`status: disabled`), kept exactly as read,
//!   written once as a reply scheduled for 2099, and only once that has settled (five matching
//!   reads a minute apart) `PATCH`ed back verbatim; the setting must then settle on the snapshot
//!   the same way and still match it five minutes later.
//! - **Nothing of the mailbox is printed**, on success or failure: comparisons report counts of
//!   differing rules or fields, never their values.
//!
//! Settled by its first run (`graph.md`): a stale `If-Match` on a rule is ignored, `sequence` is a
//! dense position, clamped to the end, a `PATCH` of `sequence` alone works, a `POST` without a
//! name is accepted and named by Graph, an id with `=` goes as `%3D`, and `null` clears nothing.
//! It prints only statuses, counts and numbers.

mod live_writes_support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    mail::{Keyword, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, Match, RuleBody,
        RuleName, TextOp, TextTest,
    },
};
use engine_provider::Provider;
use live_writes_support::{account, failed, provider, send, settled_by, token};
use serde_json::{Value, json};
const RULES: &str = "/me/mailFolders/inbox/messageRules";

/// A marker no other rule holds, and none of this test's runs holds inside another's.
fn marker() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("engine-live-rule-{nanos}-end")
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

/// Three matching reads ten seconds apart: enough for the rules' listing.
async fn settled<T, F, R>(what: &str, read: F, holds: impl Fn(&T) -> bool) -> T
where
    F: Fn() -> R,
    R: std::future::Future<Output = T>,
{
    settled_by(what, 3, Duration::from_secs(10), read, holds).await
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

/// Whether two rules say the same, whatever order their conditions and actions are in: Graph
/// keeps each as a key of one object, and the reader lists them in key order.
fn same(a: &MailRule, b: &MailRule) -> bool {
    let (RuleBody::Typed(x), RuleBody::Typed(y)) = (&a.body, &b.body) else {
        return a == b;
    };
    let alike =
        |p: &[Condition], q: &[Condition]| p.len() == q.len() && p.iter().all(|c| q.contains(c));
    a.name == b.name
        && a.enabled == b.enabled
        && x.stop == y.stop
        && x.when.mode == y.when.mode
        && alike(&x.when.conditions, &y.when.conditions)
        && alike(&x.when.except, &y.when.except)
        && x.actions.len() == y.actions.len()
        && x.actions.iter().all(|action| y.actions.contains(action))
}

/// The addresses of a rule's `fromAddresses` condition.
fn senders(rule: &Value) -> usize {
    rule["conditions"]["fromAddresses"]
        .as_array()
        .map_or(0, Vec::len)
}

fn find<'a>(listed: &'a [Value], marker: &str, wanted: &str) -> Option<&'a Value> {
    listed
        .iter()
        .find(|rule| tag(rule, marker).as_deref() == Some(wanted))
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
    assert!(
        numbers == dense,
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
    let identical = serde_json::to_string(&before).ok() == serde_json::to_string(&after).ok();
    eprintln!("rules differing from the snapshot: {changed}; byte for byte: {identical}");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
    assert!(
        changed == 0 && identical,
        "{changed} rules differ from the snapshot"
    );
}

async fn rule_cycle(token: String, marker: String, count: usize) {
    let graph = provider(&token);
    let rules = graph.mail_rules().unwrap();
    let order = |wanted: &'static [&'static str]| {
        let marker = marker.clone();
        move |listed: &Vec<Value>| tags(listed, &marker) == wanted
    };
    let from = |addresses: &[&str]| Condition::Address {
        field: AddressField::From,
        part: AddressPart::All,
        test: TextTest::new(
            TextOp::Is,
            addresses.iter().map(|a| (*a).to_owned()).collect(),
        )
        .unwrap(),
    };
    let with_senders = |tag: &str, addresses: &[&str]| {
        let mut rule = subject_rule(&marker, tag, vec![mark_read()]);
        if let RuleBody::Typed(typed) = &mut rule.body {
            typed.when.conditions.push(from(addresses));
        }
        rule
    };
    let two = ["a@example.com", "b@example.com"];

    // Five rules of ours, after every other rule.
    let base = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let mut written = base.rules.clone();
    written.extend([
        subject_rule(&marker, "a", vec![mark_read()]),
        subject_rule(&marker, "b", vec![mark_read()]),
        subject_rule(&marker, "c", vec![mark_read()]),
        with_senders("d", &two),
        with_senders("e", &two),
    ]);
    let receipt = rules
        .replace_rules(&account(), &base, &written)
        .await
        .unwrap_or_else(|err| failed("create", &err));
    assert_eq!(receipt.failures, Vec::new(), "create");
    let listed = settled(
        "the created rules",
        || listing(&token),
        order(&["a", "b", "c", "d", "e"]),
    )
    .await;
    let numbers: Vec<i64> = listed
        .iter()
        .filter(|r| ours(r, &marker))
        .filter_map(|r| r["sequence"].as_i64())
        .collect();
    eprintln!("created: sequences {numbers:?}");
    let created = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read back", &err));
    for (read, sent) in created.rules[count..].iter().zip(&written[count..]) {
        let mut read = read.clone();
        read.origin = None;
        assert!(same(&read, sent), "a rule read back is not the one written");
    }
    capture("created_rule", find(&listed, &marker, "d").unwrap());

    // A rotation of three: a b c to b c a, by the engine.
    let mut rotated = created.rules.clone();
    rotated[count..count + 3].rotate_left(1);
    let receipt = rules
        .replace_rules(&account(), &created, &rotated)
        .await
        .unwrap_or_else(|err| failed("rotate", &err));
    assert_eq!(receipt.failures, Vec::new(), "rotate");
    settled(
        "the rotation",
        || listing(&token),
        order(&["b", "c", "a", "d", "e"]),
    )
    .await;

    // Does a PATCH replace a list it is sent shorter? Asked of d alone, by hand.
    let listed = listing(&token).await;
    let d_id = find(&listed, &marker, "d").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = send(
        &token,
        reqwest::Method::PATCH,
        &format!("{RULES}/{}", encode(&d_id)),
        Some(json!({"conditions": {
            "subjectContains": [format!("{marker}-d")],
            "fromAddresses": [{"emailAddress": {"address": two[0]}}]
        }})),
    )
    .await;
    let listed = settled_by(
        "the shorter list",
        3,
        Duration::from_secs(10),
        || listing(&token),
        |listed: &Vec<Value>| find(listed, &marker, "d").is_some(),
    )
    .await;
    eprintln!(
        "PATCH of a list sent shorter: {status}; addresses after: {} of 2",
        senders(find(&listed, &marker, "d").unwrap())
    );

    // The engine shortens e's list: a rule written anew in e's place.
    let now = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let mut shortened = now.rules.clone();
    let e = shortened.len() - 1;
    shortened[e] = with_senders("e", &two[..1]);
    shortened[e].origin = now.rules[e].origin;
    let receipt = rules
        .replace_rules(&account(), &now, &shortened)
        .await
        .unwrap_or_else(|err| failed("shorten", &err));
    assert_eq!(receipt.failures, Vec::new(), "shorten");
    let listed = settled(
        "the shortened rule",
        || listing(&token),
        |listed: &Vec<Value>| {
            tags(listed, &marker) == ["b", "c", "a", "d", "e"]
                && find(listed, &marker, "e").is_some_and(|e| senders(e) == 1)
        },
    )
    .await;
    capture("rule_written_anew", find(&listed, &marker, "e").unwrap());

    // A rename, PATCHed in place.
    let before_rename = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let mut renamed = before_rename.rules.clone();
    renamed[count].name = Some(RuleName::new(format!("Engine test renamed {marker}")).unwrap());
    let receipt = rules
        .replace_rules(&account(), &before_rename, &renamed)
        .await
        .unwrap_or_else(|err| failed("rename", &err));
    assert_eq!(receipt.failures, Vec::new(), "rename");
    settled(
        "the rename",
        || listing(&token),
        |listed: &Vec<Value>| {
            find(listed, &marker, "b").is_some_and(|b| {
                b["displayName"]
                    .as_str()
                    .is_some_and(|n| n.contains("renamed"))
            })
        },
    )
    .await;

    // Read before the rename, the base is stale now that the rename has settled.
    let stale = rules
        .replace_rules(&account(), &before_rename, &before_rename.rules)
        .await
        .err()
        .unwrap_or_else(|| panic!("a stale write was taken"));
    assert_eq!(stale.class(), FailureClass::Conflict);

    // Ours removed through the engine: everything read but our rules.
    let end = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
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
        .unwrap_or_else(|err| failed("remove", &err));
    assert_eq!(receipt.failures, Vec::new(), "remove");
    settled("the removal", || listing(&token), order(&[])).await;
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
