//! Gated live provider-level checks against a real Microsoft Graph account: folder
//! role resolution, message normalization, and the snapshot → delta cursor cycle
//! through the real HTTP client.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set (an OAuth bearer access token, e.g.
//! from `tools/graph-oauth`), so the offline `cargo test --workspace` stays green.
//! There is no CI harness for this (no live Microsoft account in CI); run it
//! locally:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_provider -- --nocapture
//! ```

use std::collections::BTreeSet;

use engine_core::ids::{AccountId, MailboxId};
use engine_core::mail::MailboxRole;
use engine_core::sync::SyncUpdate;
use engine_provider::Provider;
use provider_graph::{GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A provider bound to the inbox (Graph accepts the well-known alias in the URL).
fn provider(token: String) -> GraphProvider {
    let client = GraphClient::connect(token).expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn live_mail_folders_resolve_roles() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_folders_resolve_roles: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let mailboxes = provider(token)
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    assert!(mailboxes.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &mailboxes.update else {
        panic!("expected a folder snapshot");
    };
    // Roles resolve by well-known-alias id despite localized display names.
    let roles: BTreeSet<MailboxRole> = objects.iter().filter_map(|m| m.role.clone()).collect();
    assert!(roles.contains(&MailboxRole::Inbox), "inbox role resolved");
    assert!(roles.contains(&MailboxRole::Sent), "sent role resolved");
    // Every folder is top-level (parent nulled against msgfolderroot).
    assert!(objects.iter().all(|m| m.parent.is_none()));
}

#[tokio::test]
async fn live_message_snapshot_then_delta() {
    let Some(token) = token() else {
        eprintln!("skipping live_message_snapshot_then_delta: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // The initial pass is a full snapshot of the inbox.
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    assert!(snapshot.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a message snapshot");
    };
    // The deterministic seed message is present and fully normalized.
    let subjects: BTreeSet<&str> = objects
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    assert!(
        subjects.contains("Fixture: first message"),
        "seed subject missing; got {subjects:?}"
    );
    // Graph mail is single-folder, so every membership has exactly one collection.
    assert!(objects.iter().all(|m| m.mailboxes.len().get() == 1));

    // A delta from the fresh cursor is a delta (not a snapshot).
    let delta = provider
        .sync_email(&account(), Some(&snapshot.next_cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot());
}
