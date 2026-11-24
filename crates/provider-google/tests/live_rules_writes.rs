//! Gated live **writes** of the filters. Not run by default, nor by a token alone: it changes the
//! account, so it needs `GMAIL_RULES_WRITES=1`, its own gate, beside a `GOOGLE_ACCESS_TOKEN`
//! holding `gmail.settings.basic`. The reply has a suite and a gate of its own
//! (`live_reply_writes.rs`); this one never writes it.
//!
//! What it may touch, and how it leaves the account:
//!
//! - **Only filters it creates.** Each holds a marker no other filter holds, in a subject only this
//!   run's mail could carry, and does nothing that sends mail out. Before anything is written it
//!   checks that every existing filter plans unchanged against Gmail's own vocabulary, so the
//!   engine has none to rewrite; otherwise it writes nothing.
//! - **Each write settled by reads**: whether Gmail lists a filter as soon as it answers is
//!   measured here, not assumed, and printed as a count of reads.
//! - **A byte-exact snapshot** of the listing is taken first; afterwards the filters it did not
//!   create must be that snapshot exactly. Whatever happens, every filter holding the marker is
//!   deleted, and the labels it made are deleted too.
//! - **Nothing of the account is printed**: counts, statuses and reason codes only.
//!
//! It proves a create, an edit (a new filter first, then the old one deleted), a stale base
//! refused, a forward to an unverified address held back by the engine and refused by Gmail when
//! sent anyway, and asks Gmail three questions by hand: whether a filter may add two of the user's
//! labels, whether one with no criteria is stored, and whether a second identical one is.

mod gmail_settings_support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, Kept, LossReason, MailRule,
        Match, PlanMode, RuleAddress, RuleMailbox, RuleSet, SizeBound, TextOp, TextTest,
        plan_rules,
    },
};
use engine_provider::{MailRules, Provider};
use gmail_settings_support::{account, failed, gated, provider, reason, send, settled};
use reqwest::Method;
use serde_json::{Value, json};

const GATE: &str = "GMAIL_RULES_WRITES";

fn marker() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("engine-live-filter-{nanos}-end")
}

async fn listing(token: &str) -> Vec<Value> {
    let (status, page) = send(token, Method::GET, "/settings/filters", None).await;
    assert!(matches!(status, 200 | 204), "listing: {status}");
    let mut filters = page["filter"].as_array().cloned().unwrap_or_default();
    filters.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    filters
}

fn ours(filter: &Value, marker: &str) -> bool {
    filter["criteria"].to_string().contains(marker)
}

fn theirs(filters: &[Value], marker: &str) -> Vec<Value> {
    filters
        .iter()
        .filter(|f| !ours(f, marker))
        .cloned()
        .collect()
}

fn text(values: &[String]) -> TextTest {
    TextTest::new(TextOp::Contains, values.to_vec()).unwrap()
}

fn subject(values: &[String]) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: text(values),
    }
}

fn from(value: &str) -> Condition {
    Condition::Address {
        field: AddressField::From,
        part: AddressPart::All,
        test: text(&[value.to_owned()]),
    }
}

/// This run's rules: each matches only mail whose subject holds the marker.
fn rules(marker: &str) -> Vec<MailRule> {
    let tag = |t: &str| format!("{marker}-{t}");
    vec![
        MailRule::typed(
            Match::all(vec![from("a@example.com"), subject(&[tag("a")])]),
            vec![Action::AddKeywords(
                [
                    Keyword::system(SystemKeyword::Flagged),
                    Keyword::system(SystemKeyword::Seen),
                ]
                .into(),
            )],
        ),
        MailRule::typed(
            Match::all(vec![
                subject(&[tag("b1"), tag("b2")]),
                Condition::Size(SizeBound::Over(5_000_000)),
            ])
            .except(vec![Condition::HasAttachment, from("boss@example.com")]),
            vec![
                Action::FileInto {
                    mailbox: RuleMailbox::Role(MailboxRole::Archive),
                    copy: false,
                },
                Action::MarkImportance(Importance::High),
            ],
        ),
    ]
}

/// The run's rules as read, without their origin, in the order written.
fn read_back(set: &RuleSet, marker: &str, raw: &[Value]) -> Vec<MailRule> {
    let mut found: Vec<(String, MailRule)> = raw
        .iter()
        .zip(&set.rules)
        .filter(|(filter, _)| ours(filter, marker))
        .map(|(filter, rule)| {
            let mut rule = rule.clone();
            rule.origin = None;
            (filter["criteria"].to_string(), rule)
        })
        .collect();
    found.sort_by_key(|(criteria, _)| !criteria.contains(&format!("{marker}-a")));
    found.into_iter().map(|(_, rule)| rule).collect()
}

#[tokio::test]
async fn live_filters_are_written_and_every_filter_the_test_did_not_create_stays_byte_for_byte() {
    let Some(token) = gated(GATE) else {
        eprintln!("skipping: {GATE}=1 and GOOGLE_ACCESS_TOKEN not both set");
        return;
    };
    let marker = marker();
    let before = listing(&token).await;
    let gmail = provider(&token);
    let mail_rules = gmail.mail_rules().unwrap();
    let base = mail_rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let vocabulary = mail_rules
        .rule_vocabulary(&account())
        .await
        .unwrap_or_else(|err| failed("vocabulary", &err));
    assert!(
        plan_rules(&base.rules, &vocabulary, PlanMode::Edit).written() == base.rules,
        "unexpected state: an existing filter does not plan unchanged; nothing was written"
    );
    let run = tokio::spawn(cycle(token.clone(), marker.clone())).await;
    let labels = tokio::spawn(by_hand(token.clone(), marker.clone())).await;
    clean_up(&token, &marker).await;
    let after = listing(&token).await;
    let identical =
        serde_json::to_string(&before).ok() == serde_json::to_string(&theirs(&after, &marker)).ok();
    eprintln!(
        "filters before: {}; after: {}; byte for byte: {identical}",
        before.len(),
        after.len()
    );
    for outcome in [run, labels] {
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic.into_panic());
        }
    }
    assert!(identical, "the filters differ from the snapshot");
}

async fn cycle(token: String, marker: String) {
    let gmail = provider(&token);
    let mail_rules = gmail.mail_rules().unwrap();
    let base = mail_rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let mut written = base.rules.clone();
    written.extend(rules(&marker));
    let receipt = mail_rules
        .replace_rules(&account(), &base, &written)
        .await
        .unwrap_or_else(|err| failed("create", &err));
    assert_eq!(receipt.failures, Vec::new(), "create");
    let created = settled(
        "the create",
        3,
        Duration::from_secs(2),
        || listing(&token),
        |l: &Vec<Value>| l.iter().filter(|f| ours(f, &marker)).count() == 2,
    )
    .await;
    eprintln!(
        "create: reads before listed: {}; flaps: {}",
        created.reads_before, created.flaps
    );
    capture("filters_created", &created.state, &marker);

    let read = mail_rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read back", &err));
    let raw: Vec<Value> = serde_json::from_str(read.raw.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(
        read_back(&read, &marker, &raw),
        rules(&marker),
        "read back typed"
    );

    // An edit: a new filter first, then the old one deleted.
    let mut edited = read.rules.clone();
    let at = raw
        .iter()
        .position(|f| f["criteria"].to_string().contains(&format!("{marker}-a")))
        .unwrap();
    edited[at].body = MailRule::typed(
        Match::all(vec![
            from("a@example.com"),
            subject(&[format!("{marker}-a2")]),
        ]),
        vec![Action::AddKeywords(
            [Keyword::system(SystemKeyword::Flagged)].into(),
        )],
    )
    .body;
    let receipt = mail_rules
        .replace_rules(&account(), &read, &edited)
        .await
        .unwrap_or_else(|err| failed("edit", &err));
    assert_eq!(receipt.failures, Vec::new(), "edit");
    let settled_edit = settled(
        "the edit",
        3,
        Duration::from_secs(2),
        || listing(&token),
        |l: &Vec<Value>| {
            let ours: Vec<String> = l
                .iter()
                .filter(|f| ours(f, &marker))
                .map(|f| f["criteria"].to_string())
                .collect();
            ours.len() == 2 && ours.iter().any(|c| c.contains(&format!("{marker}-a2")))
        },
    )
    .await;
    eprintln!(
        "edit: reads before listed: {}; flaps: {}",
        settled_edit.reads_before, settled_edit.flaps
    );

    // The base read before the edit is stale now.
    let err = mail_rules
        .replace_rules(&account(), &read, &read.rules)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict, "stale base");

    forward_unverified(&token, &marker, mail_rules).await;
}

/// A forward to an address the account never verified: held back by the engine before sending,
/// and refused by Gmail when sent anyway (the absence is shown against the request that asks).
async fn forward_unverified(token: &str, marker: &str, mail_rules: &dyn MailRules) {
    let unverified = "nobody@example.invalid";
    let base = mail_rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let mut written = base.rules.clone();
    written.push(MailRule::typed(
        Match::all(vec![subject(&[format!("{marker}-f")])]),
        vec![Action::Redirect {
            to: vec![RuleAddress::new(unverified).unwrap()],
            copy: true,
        }],
    ));
    let receipt = mail_rules
        .replace_rules(&account(), &base, &written)
        .await
        .unwrap_or_else(|err| failed("forward", &err));
    let outcome = receipt.outcomes.last().unwrap();
    assert_eq!(outcome.kept, Kept::NotKept);
    assert!(
        outcome
            .losses
            .iter()
            .any(|loss| loss.reason == LossReason::ForwardNotAllowed)
    );
    let body =
        json!({"criteria": {"subject": format!("{marker}-f")}, "action": {"forward": unverified}});
    let (status, answer) = send(token, Method::POST, "/settings/filters", Some(&body)).await;
    eprintln!(
        "forward to an unverified address, sent anyway: {status} {}",
        reason(&answer)
    );
    assert!(
        (400..500).contains(&status),
        "Gmail stored a forward to an unverified address"
    );
}

/// Questions asked of Gmail by hand, each with a filter or label of this run's only.
async fn by_hand(token: String, marker: String) {
    let mut labels = Vec::new();
    for n in 1..=2 {
        let body = json!({"name": format!("{marker}-L{n}"), "labelListVisibility": "labelHide", "messageListVisibility": "hide"});
        let (status, label) = send(&token, Method::POST, "/labels", Some(&body)).await;
        assert_eq!(status, 200, "label create");
        labels.push(label["id"].as_str().unwrap().to_owned());
    }
    let two = json!({"criteria": {"subject": format!("{marker}-two")}, "action": {"addLabelIds": labels}});
    let (status, answer) = send(&token, Method::POST, "/settings/filters", Some(&two)).await;
    eprintln!(
        "a filter adding two user labels: {status} {}",
        reason(&answer)
    );
    if let Some(id) = answer["id"]
        .as_str()
        .filter(|_| (200..300).contains(&status))
    {
        send(
            &token,
            Method::DELETE,
            &format!("/settings/filters/{id}"),
            None,
        )
        .await;
    }
    let empty = json!({"criteria": {}, "action": {"addLabelIds": ["STARRED"]}});
    let (status, answer) = send(&token, Method::POST, "/settings/filters", Some(&empty)).await;
    eprintln!("a filter with no criteria: {status} {}", reason(&answer));
    if let Some(id) = answer["id"]
        .as_str()
        .filter(|_| (200..300).contains(&status))
    {
        send(
            &token,
            Method::DELETE,
            &format!("/settings/filters/{id}"),
            None,
        )
        .await;
    }
    let same = json!({"criteria": {"subject": format!("{marker}-same")}, "action": {"addLabelIds": ["STARRED"]}});
    let first = send(&token, Method::POST, "/settings/filters", Some(&same))
        .await
        .0;
    let (status, answer) = send(&token, Method::POST, "/settings/filters", Some(&same)).await;
    eprintln!(
        "a second identical filter: {first} then {status} {}",
        reason(&answer)
    );
    for id in labels {
        let (status, _) = send(&token, Method::DELETE, &format!("/labels/{id}"), None).await;
        assert!((200..300).contains(&status), "label delete");
    }
}

/// Deletes every filter holding the marker, and reads until none is listed.
async fn clean_up(token: &str, marker: &str) {
    for filter in listing(token).await.iter().filter(|f| ours(f, marker)) {
        let id = filter["id"].as_str().unwrap();
        send(
            token,
            Method::DELETE,
            &format!("/settings/filters/{id}"),
            None,
        )
        .await;
    }
    settled(
        "the clean-up",
        2,
        Duration::from_secs(2),
        || listing(token),
        |l: &Vec<Value>| !l.iter().any(|f| ours(f, marker)),
    )
    .await;
}

/// Writes this run's own filters, ids replaced, where `GMAIL_RULES_CAPTURE_DIR` names a folder.
fn capture(name: &str, listed: &[Value], marker: &str) {
    let Ok(dir) = std::env::var("GMAIL_RULES_CAPTURE_DIR") else {
        return;
    };
    let ours: Vec<Value> = listed
        .iter()
        .filter(|f| ours(f, marker))
        .enumerate()
        .map(|(n, f)| {
            let mut f = f.clone();
            f["id"] = json!(format!("filter-{n}"));
            f
        })
        .collect();
    let text = json!({ "filter": ours })
        .to_string()
        .replace(marker, "engine-live-filter");
    let _ = std::fs::write(format!("{dir}/{name}.json"), text);
}
