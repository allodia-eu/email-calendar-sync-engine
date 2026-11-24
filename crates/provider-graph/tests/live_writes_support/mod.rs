//! What the gated live write suites share (`live_rules_writes.rs`, `live_reply_writes.rs`): the
//! gate, the provider, raw requests, and reading until a write has settled. A failure is reported
//! by class alone, never by the server's words.

#![allow(dead_code)]

use std::time::Duration;

use engine_core::ids::{AccountId, MailboxId};
use provider_graph::{GraphClient, GraphProvider};
use serde_json::Value;

pub(crate) const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// Panics with the failure's class alone: a provider error's detail can carry the server's
/// words, which can echo the mailbox's content.
pub(crate) fn failed<T>(what: &str, err: &engine_provider::ProviderError) -> T {
    panic!("{what} failed: {:?}", err.class())
}

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The delegated token, where writes were asked for.
pub(crate) fn token() -> Option<String> {
    let allowed = std::env::var("GRAPH_RULES_WRITES").is_ok_and(|v| v == "1");
    let token = std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    allowed.then_some(token).flatten()
}

pub(crate) fn provider(token: &str) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

pub(crate) fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

pub(crate) async fn send(
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

/// Reads with `read` until `holds` is true of `reads` reads in a row, `gap` apart, within
/// `reads` × `gap` × 10: Graph applies a write some time after answering it.
pub(crate) async fn settled_by<T, F, R>(
    what: &str,
    reads: u32,
    gap: Duration,
    read: F,
    holds: impl Fn(&T) -> bool,
) -> T
where
    F: Fn() -> R,
    R: std::future::Future<Output = T>,
{
    let mut streak = 0;
    for _ in 0..reads * 10 {
        let now = read().await;
        streak = if holds(&now) { streak + 1 } else { 0 };
        if streak == reads {
            return now;
        }
        tokio::time::sleep(gap).await;
    }
    panic!("{what} did not settle");
}
