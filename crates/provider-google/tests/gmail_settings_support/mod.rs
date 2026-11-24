//! What the gated live suites of the filters and the vacation reply share (`live_rules.rs`,
//! `live_rules_writes.rs`, `live_reply_writes.rs`): the gates, the provider, raw requests, and
//! reading until a write has settled. A failure is reported by class alone, never by the server's
//! words, and nothing of the mailbox is printed.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use std::time::Duration;

use engine_core::ids::AccountId;
use provider_google::{GmailProvider, GoogleClient};
use serde_json::Value;

/// The Gmail API root every raw request is made under.
pub(crate) const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// Panics with the failure's class alone: a provider error's detail can carry the server's
/// words, which can echo the mailbox's content.
pub(crate) fn failed<T>(what: &str, err: &engine_provider::ProviderError) -> T {
    panic!("{what} failed: {:?}", err.class())
}

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip.
pub(crate) fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
}

/// The token, where the variable `gate` reads `1` too: each write suite has its own gate, so
/// asking for the filters' writes never writes the reply.
pub(crate) fn gated(gate: &str) -> Option<String> {
    let open = std::env::var(gate).is_ok_and(|value| value == "1");
    token().filter(|_| open)
}

pub(crate) fn provider(token: &str) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GmailProvider::new(client)
}

fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

/// A raw request under [`GMAIL`]: its status and its JSON body (`Null` for none).
pub(crate) async fn send(
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut request = http()
        .request(method, format!("{GMAIL}{path}"))
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

/// A refusal's machine reason (`errors[0].reason`, else `status`): a code, never the message.
pub(crate) fn reason(body: &Value) -> String {
    let error = &body["error"];
    error["errors"][0]["reason"]
        .as_str()
        .or_else(|| error["status"].as_str())
        .unwrap_or("none")
        .to_owned()
}

/// What settling a write found: the state, how many reads it took before the first that showed
/// it (0: the read straight after the write already did), and how often a read showed it and the
/// next did not.
pub(crate) struct Settled<T> {
    pub(crate) state: T,
    pub(crate) reads_before: u32,
    pub(crate) flaps: u32,
}

/// Reads with `read` until `holds` is true of `streak` reads in a row, `gap` apart, giving up
/// after `streak` × 10 reads: whether Gmail shows a write at once is what this measures, so
/// nothing assumes it.
pub(crate) async fn settled<T, F, R>(
    what: &str,
    streak: u32,
    gap: Duration,
    read: F,
    holds: impl Fn(&T) -> bool,
) -> Settled<T>
where
    F: Fn() -> R,
    R: std::future::Future<Output = T>,
{
    let mut run = 0;
    let mut first = None;
    let mut flaps = 0;
    for attempt in 0..streak * 10 {
        let now = read().await;
        if holds(&now) {
            first.get_or_insert(attempt);
            run += 1;
        } else {
            if run > 0 {
                flaps += 1;
            }
            run = 0;
        }
        if run == streak {
            return Settled {
                state: now,
                reads_before: first.unwrap_or(attempt),
                flaps,
            };
        }
        tokio::time::sleep(gap).await;
    }
    panic!("{what} did not settle");
}
