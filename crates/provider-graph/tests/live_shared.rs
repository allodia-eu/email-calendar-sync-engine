//! Gated live shared-mailbox checks against a real Microsoft 365 tenant.
//!
//! These need a **work/school** account: shared mailboxes and the `*.Shared` delegated
//! scopes are an Exchange Online feature, and a personal Microsoft account has neither. So
//! they are gated on `GRAPH_SHARED_MAILBOX` in addition to `GRAPH_ACCESS_TOKEN`, and skip
//! (rather than fail) for the personal-account token the rest of the live suite uses:
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml \
//!     -- token --profile work)" \
//! GRAPH_SHARED_MAILBOX="belmar-orderbevestigingen@fits4all.nl" \
//!   cargo test -p provider-graph --test live_shared -- --nocapture
//! ```
//!
//! Everything here is **read-only**: one folder-id `GET` per case. The mailbox named by
//! `GRAPH_SHARED_MAILBOX` must be a throwaway one, and nothing in this file writes to it.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::{Provider, SharedMailboxes};
use provider_graph::{GraphClient, GraphProvider};

/// The bearer token, or `None` to skip.
fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// The shared mailbox to probe. Its absence is the skip signal for a personal-account run.
fn shared_mailbox() -> Option<String> {
    std::env::var("GRAPH_SHARED_MAILBOX")
        .ok()
        .filter(|a| !a.is_empty())
}

fn provider(token: String) -> GraphProvider {
    let client =
        GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

/// Both gates, or `None` (skipping, with a note).
fn gated(test: &str) -> Option<(GraphProvider, String)> {
    let (Some(token), Some(shared)) = (token(), shared_mailbox()) else {
        eprintln!(
            "skipping {test}: set GRAPH_ACCESS_TOKEN and GRAPH_SHARED_MAILBOX (work account)"
        );
        return None;
    };
    Some((provider(token), shared))
}

#[tokio::test]
async fn live_resolving_a_granted_shared_mailbox_succeeds() {
    let Some((provider, shared)) = gated("live_resolving_a_granted_shared_mailbox_succeeds") else {
        return;
    };

    // Graph has no list API, and the capability says so — a host must ask for an address.
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::ByAddress
    );
    assert_eq!(
        provider.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );

    let resolved = provider
        .resolve_shared_mailbox(&shared)
        .await
        .expect("the mailbox is shared with this credential");
    // The handle is the address, because that is what `/users/{…}` takes.
    assert_eq!(resolved.handle.as_str(), shared);
    assert_eq!(resolved.address.as_deref(), Some(shared.as_str()));
    assert!(!resolved.personal);

    // Case-insensitive: an address a user types is not normalized, and Graph resolves it
    // either way.
    assert!(
        provider
            .resolve_shared_mailbox(&shared.to_uppercase())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn live_an_unresolvable_address_is_terminal_not_retryable() {
    let Some((provider, shared)) = gated("live_an_unresolvable_address_is_terminal_not_retryable")
    else {
        return;
    };
    // A nonexistent local part in the same domain as the known-good mailbox, so the failure
    // is about the mailbox rather than about the domain. Graph answers
    // `404 ErrorInvalidUser`.
    let domain = shared.rsplit_once('@').map_or_else(
        || "example.invalid".to_owned(),
        |(_, domain)| domain.to_owned(),
    );
    let missing = format!("no-such-mailbox-e2e-probe@{domain}");

    let err = provider
        .resolve_shared_mailbox(&missing)
        .await
        .expect_err("no mailbox at that address");
    // Terminal: a host must show "check the address", never retry. This is also the class a
    // mailbox that exists but is *not shared* lands in — Graph will not disclose the
    // difference on this route (`graph.md`).
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(!err.is_retryable());
    assert!(err.detail().contains(&missing), "{err}");
}

#[tokio::test]
async fn live_a_path_traversal_never_resolves_to_the_signed_in_mailbox() {
    let Some((provider, _)) =
        gated("live_a_path_traversal_never_resolves_to_the_signed_in_mailbox")
    else {
        return;
    };
    // This is the case that made validation non-optional, and it is worth stating plainly
    // because the intuitive answer is wrong. Percent-encoding `../me` gives `..%2Fme`, which
    // *looks* inert — and `GET /v1.0/users/..%2Fme/mailFolders/inbox` was observed answering
    // **200 with the signed-in user's own Inbox**: Graph decodes the segment and re-resolves
    // the path. So a resolver that only encoded would confirm `../me` as a shared mailbox,
    // and a host would onboard its own inbox under someone else's name.
    //
    // The address is therefore refused before any request is made.
    for hostile in ["../me", "..%2Fme", "a@b.test/../../me"] {
        match provider.resolve_shared_mailbox(hostile).await {
            Ok(resolved) => panic!("{hostile:?} must never resolve, got {resolved:?}"),
            Err(err) => assert_eq!(err.class(), FailureClass::Permanent, "{hostile}: {err}"),
        }
    }
}
