//! Gated live checks for the mailbox's own sender identity against a real Microsoft account.
//!
//! Read-only, and the point is to prove that is the right shape. Two things a fake cannot say:
//!
//! - that `?$select=displayName,mail,userPrincipalName` on the mailbox principal is a request Graph
//!   accepts with the delegated scopes this adapter already holds, so reading the name costs a host
//!   no new consent;
//! - that the resource really carries a `displayName` for the signed-in mailbox, which is what lets
//!   a host fill its "your name" field in instead of asking.
//!
//! There is deliberately **no** write here. A mailbox's display name is a directory attribute a
//! tenant administrator owns, which is why the capability is `ReadOnly`; the adapter implements
//! no write path, and the assertion below pins that rather than leaving it to be noticed later.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set.
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-graph --test live_identity -- --nocapture
//! ```

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::{IdentityControls, Provider, SenderIdentityId};
use provider_graph::{GraphClient, GraphProvider};

/// The test account's own address.
const SELF_ADDRESS: &str = "allodia-e2e@outlook.com";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn the_mailbox_reports_one_identity_carrying_its_directory_name() {
    let Some(token) = token() else {
        eprintln!("skipping live identity read: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    assert_eq!(
        provider.connection_info().capabilities.sender_identities(),
        Some(IdentityControls::ReadOnly)
    );

    let identities = provider
        .sender_identities(&account())
        .await
        .expect("reading the mailbox principal");

    // The principal *is* the mailbox, so there is exactly one identity however many addresses
    // the directory carries for it.
    assert_eq!(identities.len(), 1);
    let identity = &identities[0];
    assert!(
        identity.address.email.eq_ignore_ascii_case(SELF_ADDRESS),
        "expected the account's own address, got {}",
        identity.address.email
    );
    // The whole reason a host reads this: a name it would otherwise have to ask for.
    assert!(
        identity.address.name.is_some(),
        "the directory holds a display name for this mailbox"
    );
}

#[tokio::test]
async fn a_rename_is_refused_rather_than_attempted() {
    let Some(token) = token() else {
        eprintln!("skipping live identity write refusal: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // Not a limitation being documented: an account holder changing a directory attribute is
    // somebody else's operation, so the adapter must refuse locally rather than send a request
    // that would fail at the tenant.
    let refused = provider
        .set_sender_name(&account(), &SenderIdentityId::new(SELF_ADDRESS), "Probe")
        .await;
    assert!(refused.is_err(), "a read-only directory accepts no rename");
}
