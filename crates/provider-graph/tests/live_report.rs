//! Gated live checks for `reportMessage` against a real Microsoft Graph account.
//!
//! Every claim the adapter's module docs make about this endpoint was established
//! here, and three of them contradict Microsoft's published documentation — so these
//! are the tests that would notice if the service ever started behaving as documented
//! (or changed again). The offline suite replays captured bytes and cannot.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set. There is no CI harness for this; run it
//! locally:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_report -- --nocapture --test-threads=1
//! ```

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::{MessageReport, Provider, ReportEvidence, ReportVerdict, ReportingProvider};
use provider_graph::{GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GraphProvider {
    let client =
        GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

/// The id of the folder carrying `role`.
async fn folder_with_role(provider: &GraphProvider, role: &MailboxRole) -> MailboxId {
    let folders = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("folders");
    let SyncUpdate::Snapshot { objects, .. } = folders.update else {
        panic!("expected a folder snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.role.as_ref() == Some(role))
        .unwrap_or_else(|| panic!("no folder with role {role:?}"))
        .id
}

/// The messages currently in the bound folder.
async fn messages(provider: &GraphProvider) -> Vec<Message> {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    match emails.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { .. } => panic!("a first pass is a snapshot"),
    }
}

#[tokio::test]
async fn a_junk_report_is_acknowledged_and_files_the_message_and_not_junk_returns_it() {
    let Some(token) = token() else {
        eprintln!("skipping live Graph report test: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // Graph is the only transport that acknowledges a report; if that ever stops being
    // true the capability is lying to every host that reads it.
    let controls = provider
        .connection_info()
        .capabilities
        .mail_report()
        .expect("Graph advertises reporting");
    assert_eq!(controls.evidence, ReportEvidence::Acknowledged);
    assert!(controls.verdicts.phishing, "Graph has a phishing verdict");

    let junk = folder_with_role(&provider, &MailboxRole::Junk).await;
    let inbox = folder_with_role(&provider, &MailboxRole::Inbox).await;

    let target = messages(&provider)
        .await
        .into_iter()
        .next()
        .expect("an inbox message to report")
        .id;
    let key = ProviderKey::new(target.as_str()).expect("a synced id is a provider key");

    let receipt = provider
        .report_message(
            &account(),
            &MessageReport::new(key.clone(), ReportVerdict::Junk, junk),
        )
        .await
        .expect("report junk");

    // The immutable id survives the server-side move. Without
    // `Prefer: IdType="ImmutableId"` on every request this key would 404 immediately,
    // and the outbox receipt would name a message nothing can find.
    assert_eq!(receipt.message_key, key);
    assert!(
        !messages(&provider).await.iter().any(|m| m.id == target),
        "the report filed the message out of the Inbox"
    );

    // Restore, and prove the not-junk direction brings it back.
    provider
        .report_message(
            &account(),
            &MessageReport::new(key.clone(), ReportVerdict::NotJunk, inbox),
        )
        .await
        .expect("report not junk");
    assert!(
        messages(&provider).await.iter().any(|m| m.id == target),
        "not-junk returned the message to the Inbox"
    );
}

#[tokio::test]
async fn reporting_the_same_message_twice_is_accepted() {
    // The outbox may retry, so a repeated report must not be an error.
    let Some(token) = token() else {
        eprintln!("skipping live Graph idempotency test: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let inbox = folder_with_role(&provider, &MailboxRole::Inbox).await;
    let target = messages(&provider)
        .await
        .into_iter()
        .next()
        .expect("an inbox message")
        .id;
    let key = ProviderKey::new(target.as_str()).unwrap();

    for attempt in 1..=2 {
        provider
            .report_message(
                &account(),
                &MessageReport::new(key.clone(), ReportVerdict::NotJunk, inbox.clone()),
            )
            .await
            .unwrap_or_else(|err| panic!("attempt {attempt} must be accepted: {err}"));
    }
}
