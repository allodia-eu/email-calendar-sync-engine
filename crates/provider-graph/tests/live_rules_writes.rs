//! Gated live **writes** of the inbox rules and the automatic replies. Not run by default, nor by
//! a token alone: it changes the mailbox, so it needs `GRAPH_RULES_WRITES=1` beside a delegated
//! `GRAPH_ACCESS_TOKEN` with `MailboxSettings.ReadWrite`.
//!
//! What it touches, and how it leaves the mailbox:
//!
//! - **Rules.** One rule of its own, named and tested by a marker no other rule holds, placed last,
//!   that only marks mail read. It is created, renamed and removed through `replace_rules`; every
//!   other rule must read back exactly as it was. Whatever happens, the rules named with the marker
//!   are removed at the end, and nothing else is written.
//! - **`If-Match` on a rule.** A `PATCH` of that rule with a stale `If-Match`, to see whether Graph
//!   honours it (a `412`) or ignores it (the `PATCH` lands).
//! - **The reply.** Its setting exactly as read is kept; a reply scheduled decades ahead is written
//!   and read back; then the kept setting is `PATCH`ed back verbatim, and the version read
//!   afterwards must be the one first read.

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Keyword, SystemKeyword},
    rules::{Action, Condition, HeaderField, MailRule, Match, RuleName, TextOp, TextTest},
    time::UtcDateTime,
    vacation::{AutoReply, VacationWindow},
};
use engine_provider::{Provider, VacationPrecondition};
use provider_graph::{GraphClient, GraphProvider};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

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

fn named(rule: &MailRule, marker: &str) -> bool {
    rule.name
        .as_ref()
        .is_some_and(|name| name.as_str().contains(marker))
}

/// A rule as read, without where it was read from: what must not change across a write.
fn content(rule: &MailRule) -> MailRule {
    let mut rule = rule.clone();
    rule.origin = None;
    rule
}

#[tokio::test]
async fn live_a_rule_is_created_renamed_and_removed_leaving_every_other_rule_as_it_was() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let marker = marker();
    let run = tokio::spawn(rule_cycle(token.clone(), marker.clone())).await;
    // Whatever happened, no rule named with the marker stays.
    remove_marked(&token, &marker).await;
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn rule_cycle(token: String, marker: String) {
    let graph = provider(&token);
    let rules = graph.mail_rules().unwrap();
    let base = rules.read_rules(&account()).await.expect("read");
    let theirs: Vec<MailRule> = base.rules.iter().map(content).collect();
    let mine = MailRule::typed(
        Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test: TextTest::new(TextOp::Contains, vec![marker.clone()]).unwrap(),
        }]),
        vec![Action::AddKeywords(
            [Keyword::system(SystemKeyword::Seen)].into(),
        )],
    )
    .named(RuleName::new(format!("Engine test {marker}")).unwrap());
    let mut written = base.rules.clone();
    written.push(mine.clone());
    let receipt = rules
        .replace_rules(&account(), &base, &written)
        .await
        .expect("create");
    assert_eq!(receipt.failures, Vec::new());
    let created = rules.read_rules(&account()).await.expect("read back");
    assert_eq!(receipt.version, created.version);
    let (last, before) = created.rules.split_last().expect("a rule");
    assert!(named(last, &marker), "created last");
    assert_eq!(content(last).name, mine.name, "read back with its name");
    assert_eq!(before.iter().map(content).collect::<Vec<_>>(), theirs);

    if_match_probe(&token, &marker).await;

    // Renamed: a PATCH of the rule alone.
    let created = rules.read_rules(&account()).await.expect("read");
    let mut renamed = created.rules.clone();
    let mine = renamed.last_mut().unwrap();
    mine.name = Some(RuleName::new(format!("Engine test renamed {marker}")).unwrap());
    let receipt = rules
        .replace_rules(&account(), &created, &renamed)
        .await
        .expect("rename");
    assert_eq!(receipt.failures, Vec::new());
    let after = rules.read_rules(&account()).await.expect("read");
    assert!(
        after
            .rules
            .last()
            .unwrap()
            .name
            .as_ref()
            .unwrap()
            .as_str()
            .contains("renamed")
    );

    // A stale base is refused, and nothing is written.
    let stale = rules
        .replace_rules(&account(), &created, &created.rules)
        .await
        .unwrap_err();
    assert_eq!(stale.class(), engine_core::error::FailureClass::Conflict);

    // Removed: a DELETE, and everyone else's rules as they were.
    let without: Vec<MailRule> = after.rules[..after.rules.len() - 1].to_vec();
    let receipt = rules
        .replace_rules(&account(), &after, &without)
        .await
        .expect("remove");
    assert_eq!(receipt.failures, Vec::new());
    let end = rules.read_rules(&account()).await.expect("read");
    assert_eq!(end.rules.iter().map(content).collect::<Vec<_>>(), theirs);
    assert_eq!(end.version, base.version, "the listing is as it was");
}

/// Sends a `PATCH` of the marked rule's name with a stale `If-Match`, and reports whether Graph
/// refused it. The rule has no ETag to send a fresh one with, so this is the one question to ask.
async fn if_match_probe(token: &str, marker: &str) {
    let rules: serde_json::Value = http()
        .get(format!("{GRAPH}/me/mailFolders/inbox/messageRules"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mine = rules["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| {
            rule["displayName"]
                .as_str()
                .is_some_and(|n| n.contains(marker))
        })
        .expect("the marked rule");
    assert!(
        mine.get("@odata.etag").is_none(),
        "a rule now carries an ETag"
    );
    let id = mine["id"].as_str().unwrap();
    let status = http()
        .patch(format!("{GRAPH}/me/mailFolders/inbox/messageRules/{id}"))
        .bearer_auth(token)
        .header("If-Match", "W/\"stale-etag\"")
        .header("Content-Type", "application/json")
        .body(format!(
            r#"{{"displayName":"Engine test probed {marker}"}}"#
        ))
        .send()
        .await
        .unwrap()
        .status();
    eprintln!("PATCH of a rule with a stale If-Match answered {status}");
    assert!(
        status.is_success() || status.as_u16() == 412,
        "neither landed nor refused as stale: {status}"
    );
}

async fn remove_marked(token: &str, marker: &str) {
    let rules: serde_json::Value = match http()
        .get(format!("{GRAPH}/me/mailFolders/inbox/messageRules"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(answer) => answer.json().await.unwrap_or_default(),
        Err(_) => return,
    };
    for rule in rules["value"].as_array().into_iter().flatten() {
        let ours = rule["displayName"]
            .as_str()
            .is_some_and(|name| name.contains(marker));
        if let (true, Some(id)) = (ours, rule["id"].as_str()) {
            let _ = http()
                .delete(format!("{GRAPH}/me/mailFolders/inbox/messageRules/{id}"))
                .bearer_auth(token)
                .send()
                .await;
        }
    }
}

#[tokio::test]
async fn live_a_reply_scheduled_decades_ahead_is_written_read_back_and_the_old_one_restored() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let graph = provider(&token);
    let reply = graph.vacation().unwrap();
    let before = reply.read_vacation(&account()).await.expect("read");
    let kept = before.raw.clone().expect("the setting as read");
    let run = tokio::spawn(reply_cycle(token.clone(), before.clone())).await;
    // Put the setting back exactly as it was read, whatever happened.
    let restored = http()
        .patch(format!("{GRAPH}/me/mailboxSettings"))
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .body(format!(
            r#"{{"automaticRepliesSetting":{}}}"#,
            kept.as_str()
        ))
        .send()
        .await
        .expect("restore")
        .status();
    assert!(restored.is_success(), "restore answered {restored}");
    let after = reply.read_vacation(&account()).await.expect("read");
    assert_eq!(after.version, before.version, "the setting is as it was");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
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
    // The read taken before the write is stale now.
    let stale = reply
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&before), &new)
        .await
        .unwrap_err();
    assert_eq!(stale.class(), engine_core::error::FailureClass::Conflict);
}
