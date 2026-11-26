//! Gated live check that a Graph mailbox serves a message's whole RFC 5322 source, and says so:
//! the provider advertises `message_source`, and `$value` answers the bytes of the message the
//! sync listed. Read-only: it lists the inbox and fetches one source, nothing else.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set (see `live_provider.rs` for how to mint one).

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::SyncUpdate,
};
use engine_provider::Provider;
use provider_graph::{GraphClient, GraphProvider};

#[tokio::test]
async fn live_a_message_source_is_the_listed_message() {
    let Some(token) = std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    else {
        eprintln!("skipping live_a_message_source_is_the_listed_message: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let provider = GraphProvider::new(client, MailboxId::try_from("inbox").unwrap());
    let account = AccountId::try_from("live").unwrap();

    // A host asks the capability before it relies on the fetch.
    assert!(provider.connection_info().capabilities.message_source());

    let synced = provider.sync_email(&account, None).await.expect("inbox");
    let SyncUpdate::Snapshot { objects, .. } = synced.update else {
        panic!("a first sync is a snapshot");
    };
    let message = objects
        .iter()
        .find(|m| !m.envelope.message_id.is_empty())
        .expect("the inbox holds a message with a Message-ID");

    let source = provider
        .fetch_message_source(&account, message)
        .await
        .expect("the message's source");
    let text = String::from_utf8_lossy(source.as_bytes());
    let head = text
        .split("\r\n\r\n")
        .next()
        .expect("a header section")
        .to_ascii_lowercase();
    // RFC 5322 §3.6: `Date` and `From` are the two fields every message carries.
    assert!(head.starts_with("date:") || head.contains("\r\ndate:"));
    assert!(head.starts_with("from:") || head.contains("\r\nfrom:"));
    // And it is the message the sync listed, not some other one.
    let listed = message.envelope.message_id[0].as_str().to_ascii_lowercase();
    assert!(
        head.contains(&listed),
        "the source carries the listed message's Message-ID"
    );
}
