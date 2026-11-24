//! Gated live reads of the inbox rules and the automatic replies (`crate::rules`,
//! `crate::vacation`), read-only: nothing here writes to the mailbox.
//!
//! Each test skips unless its variables are set, so the offline `cargo test --workspace` stays
//! green:
//!
//! - `GRAPH_ACCESS_TOKEN`: a delegated token with `MailboxSettings.Read`, read at `/me`.
//! - `GRAPH_APP_ACCESS_TOKEN` (`tools/graph-oauth app-token`, application `MailboxSettings.Read`)
//!   with `GRAPH_RULES_MAILBOX` and, optionally, `GRAPH_RULES_SHARED_MAILBOX`: a user's mailbox and
//!   a shared one, read at `/users/{address}`, as a transfer host holding application permissions
//!   reads them. The addresses are given at run time and never printed.
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- --profile m365 token)" \
//! GRAPH_APP_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- app-token)" \
//! GRAPH_RULES_MAILBOX=… GRAPH_RULES_SHARED_MAILBOX=… \
//!   cargo test -p provider-graph --test live_rules
//! ```

use engine_core::{
    ids::{AccountId, MailboxId},
    rules::{Kept, PlanMode, plan_rules},
};
use engine_provider::Provider;
use provider_graph::{GraphClient, GraphProvider, MailboxPrincipal};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn provider(token: String, principal: MailboxPrincipal) -> GraphProvider {
    let client = GraphClient::for_mailbox(
        token,
        principal,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

/// Reads the rules and the reply twice each, and checks what a write would rely on: the version
/// is stable while nothing changes, and every rule Graph holds plans whole against Graph's own
/// vocabulary, so writing the rules back as read changes nothing. Answers how many rules there
/// were, never what they say.
async fn read_both(graph: &GraphProvider) -> usize {
    let rules = graph.mail_rules().expect("offered");
    let vocabulary = rules.rule_vocabulary(&account()).await.unwrap();
    let first = rules.read_rules(&account()).await.expect("rules read");
    let again = rules
        .read_rules(&account())
        .await
        .expect("rules read again");
    assert!(first.version.is_some());
    assert_eq!(
        first.version, again.version,
        "the version moved with no change"
    );
    let plan = plan_rules(&first.rules, &vocabulary, PlanMode::Edit);
    for outcome in &plan.outcomes {
        assert_eq!(outcome.kept, Kept::Whole, "{outcome:?}");
    }
    let reply = graph.vacation().expect("offered");
    let read = reply.read_vacation(&account()).await.expect("reply read");
    let read_again = reply
        .read_vacation(&account())
        .await
        .expect("reply read again");
    assert!(read.version.is_some());
    assert_eq!(read.version, read_again.version);
    first.rules.len()
}

#[tokio::test]
async fn live_the_signed_in_users_rules_and_reply_read() {
    let Some(token) = var("GRAPH_ACCESS_TOKEN") else {
        eprintln!("skipping: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let count = read_both(&provider(token, MailboxPrincipal::Me)).await;
    eprintln!("read {count} rules at /me");
}

#[tokio::test]
async fn live_an_application_reads_a_users_and_a_shared_mailboxs_rules_and_reply() {
    let (Some(token), Some(mailbox)) = (var("GRAPH_APP_ACCESS_TOKEN"), var("GRAPH_RULES_MAILBOX"))
    else {
        eprintln!("skipping: GRAPH_APP_ACCESS_TOKEN or GRAPH_RULES_MAILBOX unset");
        return;
    };
    let user = MailboxPrincipal::user(mailbox).expect("an address");
    let count = read_both(&provider(token.clone(), user)).await;
    eprintln!("read {count} rules at /users/{{address}}");
    if let Some(shared) = var("GRAPH_RULES_SHARED_MAILBOX") {
        let shared = MailboxPrincipal::user(shared).expect("an address");
        let count = read_both(&provider(token, shared)).await;
        eprintln!("read {count} rules of a shared mailbox");
    }
}

#[tokio::test]
async fn live_an_application_cannot_use_me() {
    // An app-only token has no signed-in user: `/me` is refused, so a transfer host names the
    // mailbox. The refusal is classified, and carries no rule.
    let Some(token) = var("GRAPH_APP_ACCESS_TOKEN") else {
        eprintln!("skipping: GRAPH_APP_ACCESS_TOKEN unset");
        return;
    };
    let graph = provider(token, MailboxPrincipal::Me);
    let err = graph
        .mail_rules()
        .unwrap()
        .read_rules(&account())
        .await
        .unwrap_err();
    assert!(!err.class().is_retryable(), "{:?}", err.class());
}
