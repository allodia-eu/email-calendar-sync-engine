//! Gated live integration: a draft's importance against a real Microsoft Graph account.
//!
//! Two answers only Exchange gives. A draft is created from the assembled MIME, the same bytes
//! `/sendMail` takes, so the first save shows whether Exchange turns the `Importance` header into
//! the message's `importance` property. A re-save of a draft is a JSON `PATCH`, so the later saves
//! show that the property written there moves it, down to normal included. Nothing is sent.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set, so `cargo test --workspace` stays green:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_importance -- --nocapture
//! ```

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Importance},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_graph::{GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The throwaway test account's own address; nothing here is ever sent.
const SELF_ADDRESS: &str = "allodia-e2e@outlook.com";

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn drafts(token: String) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("drafts").unwrap())
}

/// The importance the account's own sync reads for the stored message `key`.
async fn stored_importance(provider: &GraphProvider, key: &ProviderKey) -> Importance {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.id.key() == key)
        .expect("the saved draft syncs back")
        .envelope
        .importance
}

#[tokio::test]
async fn live_graph_a_drafts_importance_follows_every_save() {
    let Some(token) = token() else {
        eprintln!("skipping live_graph_a_drafts_importance: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = drafts(token);
    let message_id = MessageIdHeader::new(format!(
        "live-graph-importance-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ))
    .unwrap();
    let draft = |importance| {
        Draft::new(
            message_id.clone(),
            EmailAddress::new(SELF_ADDRESS),
            vec![EmailAddress::new(SELF_ADDRESS)],
            "Importance probe",
            "Saved as a draft by the live test (never sent).",
        )
        .with_importance(importance)
    };

    // Created from MIME: the header is all Exchange is told.
    let key = provider
        .put_draft(&account(), &draft(Importance::High), None)
        .await
        .expect("first save");
    assert_eq!(stored_importance(&provider, &key).await, Importance::High);

    // Rewritten by `PATCH`, which keeps the key; normal is the case a property left out of the
    // patch would not reach.
    let mut current = key;
    for importance in [Importance::Low, Importance::Normal] {
        current = provider
            .put_draft(&account(), &draft(importance), Some(&current))
            .await
            .expect("re-save");
        assert_eq!(stored_importance(&provider, &current).await, importance);
    }

    provider
        .delete_draft(&account(), &current)
        .await
        .expect("discard");
}
