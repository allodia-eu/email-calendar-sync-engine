//! Gated live proof that a client bound to a Microsoft 365 **shared mailbox** sends as it:
//! `POST /users/{shared}/sendMail`, a request shape no other live test sends.
//!
//! **Every run sends one real email**, so this has a gate of its own on top of `live_shared.rs`'s,
//! naming the recipient. None of the three values is ever written into this file:
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml \
//!     -- --profile m365 token)" \
//! GRAPH_SHARED_MAILBOX="<a mailbox shared with that account, with Send As>" \
//! GRAPH_SHARED_SEND_TO="<an address that expects the test message>" \
//!   cargo test -p provider-graph --test live_shared_send -- --nocapture
//! ```
//!
//! Beyond the send itself it reads exactly one message back — the one it sent, found by its
//! `Message-ID` — from the shared mailbox's Sent Items and from the signed-in user's own.

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::{Draft, Provider};
use provider_graph::{GraphClient, GraphProvider, MailboxPrincipal};
use serde_json::Value;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

fn account() -> AccountId {
    AccountId::try_from("live-shared-send").unwrap()
}

fn provider(token: &str, principal: MailboxPrincipal) -> GraphProvider {
    let client = GraphClient::for_mailbox(
        token,
        principal,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("sentitems").unwrap())
}

/// The one message with `message_id` in `mailbox`'s Sent Items (`me` or `users/{address}`),
/// read as its `from`/`sender` addresses, or `None` when it is not there.
async fn sent_copy(token: &str, mailbox: &str, message_id: &str) -> Option<(String, String)> {
    let http = engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy");
    let mut url =
        reqwest::Url::parse(&format!("{GRAPH}/{mailbox}/mailFolders/sentitems/messages")).unwrap();
    url.query_pairs_mut()
        .append_pair("$filter", &format!("internetMessageId eq '<{message_id}>'"))
        // `toRecipients` too: without the recipients selected, Graph answers `sender` with the
        // delegate's X.500 `legacyExchangeDN` rather than an address (`graph.md`).
        .append_pair("$select", "from,sender,toRecipients");
    let found: Value = http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .expect("a response")
        .json()
        .await
        .expect("a JSON answer");
    let message = found["value"].as_array()?.first()?.clone();
    let address = |field: &str| {
        message[field]["emailAddress"]["address"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    Some((address("from"), address("sender")))
}

#[tokio::test]
async fn live_a_client_bound_to_a_shared_mailbox_sends_as_it() {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(token), Some(shared), Some(to)) = (
        var("GRAPH_ACCESS_TOKEN"),
        var("GRAPH_SHARED_MAILBOX"),
        var("GRAPH_SHARED_SEND_TO"),
    ) else {
        eprintln!(
            "skipping live_a_client_bound_to_a_shared_mailbox_sends_as_it: set \
             GRAPH_ACCESS_TOKEN, GRAPH_SHARED_MAILBOX and GRAPH_SHARED_SEND_TO (sends one email)"
        );
        return;
    };
    let handle = provider(&token, MailboxPrincipal::Me)
        .resolve_shared_mailbox(&shared)
        .await
        .expect("the mailbox is shared with this credential")
        .handle;
    let bound = provider(&token, MailboxPrincipal::shared(&handle).unwrap());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let domain = shared.rsplit_once('@').map(|(_, d)| d).unwrap();
    let message_id = MessageIdHeader::new(format!("graph-shared-send-{nonce}@{domain}")).unwrap();
    let draft = Draft::new(
        message_id.clone(),
        EmailAddress::new(&shared),
        vec![EmailAddress::new(&to)],
        "Engine live test: sent as a shared mailbox",
        "One message from the email-calendar-sync-engine live suite, proving that a client \
         bound to a shared mailbox sends as it. No reply needed.",
    );
    // The one send. Nothing else here may send: a `From` other than the bound mailbox is
    // refused before any request, which the offline suite proves (`submit.rs`) — a live
    // check of it could only ever cost a second email if that guard broke.
    let receipt = bound
        .submit_email(&account(), &draft)
        .await
        .expect("sendMail as the shared mailbox");
    assert_eq!(receipt.message_id, message_id);

    let mut copy = None;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        copy = sent_copy(&token, &format!("users/{shared}"), message_id.as_str()).await;
        if copy.is_some() {
            break;
        }
    }
    let (from, sender) = copy.expect("the sent copy is filed in the shared mailbox's Sent Items");
    println!("shared Sent Items copy: from {from:?}, sender {sender:?}");
    assert!(from.eq_ignore_ascii_case(&shared), "sent as {from:?}");
    let own = sent_copy(&token, "me", message_id.as_str()).await;
    println!(
        "a copy in the signed-in user's own Sent Items: {}",
        own.is_some()
    );
}
