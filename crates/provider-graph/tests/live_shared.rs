//! Gated live shared-mailbox checks against a real Microsoft 365 tenant.
//!
//! Shared mailboxes and the `*.Shared` delegated scopes are an Exchange Online feature a
//! personal Microsoft account has neither of, so these need a **work/school** token *and* the
//! address of a mailbox shared with it. They skip unless both are set, and neither is ever
//! written into this file — the mailbox is somebody's real one:
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml \
//!     -- --profile m365 token)" \
//! GRAPH_SHARED_MAILBOX="<a mailbox shared with that account>" \
//!   cargo test -p provider-graph --test live_shared -- --nocapture
//! ```
//!
//! **Read-only throughout**: mail-folder and directory `GET`s. Nothing is sent, moved or
//! written, and no message of the shared mailbox is read.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::{MailboxAccess, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Provider, SharedMailboxes};
use provider_graph::{GraphClient, GraphProvider, MailboxPrincipal};

fn account() -> AccountId {
    AccountId::try_from("live-shared").unwrap()
}

/// The token and the shared mailbox, or `None` (skipping, with a note).
fn gated(test: &str) -> Option<(String, String)> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(token), Some(shared)) = (var("GRAPH_ACCESS_TOKEN"), var("GRAPH_SHARED_MAILBOX"))
    else {
        eprintln!(
            "skipping {test}: set GRAPH_ACCESS_TOKEN and GRAPH_SHARED_MAILBOX (work account)"
        );
        return None;
    };
    Some((token, shared))
}

fn provider(token: &str, principal: MailboxPrincipal) -> GraphProvider {
    let client = GraphClient::for_mailbox(
        token,
        principal,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn live_a_granted_mailbox_resolves_and_nothing_else_does() {
    let Some((token, shared)) = gated("live_a_granted_mailbox_resolves_and_nothing_else_does")
    else {
        return;
    };
    let own = provider(&token, MailboxPrincipal::Me);
    // No route lists shared mailboxes, and the capability says so.
    assert_eq!(
        own.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::ByAddress
    );
    assert_eq!(
        own.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );

    let resolved = own
        .resolve_shared_mailbox(&shared)
        .await
        .expect("the mailbox is shared with this credential");
    assert_eq!(resolved.handle.as_str(), shared);
    // An address is not case-normalized by whoever typed it, and Graph resolves it anyway.
    assert!(
        own.resolve_shared_mailbox(&shared.to_uppercase())
            .await
            .is_ok()
    );

    // A mailbox that does not exist, in the same domain so the failure is about the
    // mailbox: terminal, so a host asks the user to check it rather than retrying.
    let domain = shared.rsplit_once('@').map(|(_, d)| d).unwrap();
    let err = own
        .resolve_shared_mailbox(&format!("no-such-mailbox-e2e-probe@{domain}"))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn live_the_credentials_own_mailbox_is_never_a_shared_one() {
    let Some((token, _)) = gated("live_the_credentials_own_mailbox_is_never_a_shared_one") else {
        return;
    };
    let own = provider(&token, MailboxPrincipal::Me);
    let identity = own
        .sender_identities(&account())
        .await
        .expect("the signed-in mailbox's own identity")
        .remove(0);
    // `/users/{own address}` answers 200 like any shared mailbox; the Inbox id is what gives
    // it away, under its own address and under that address in another case alike.
    for own_address in [
        identity.address.email.clone(),
        identity.address.email.to_uppercase(),
    ] {
        let err = own.resolve_shared_mailbox(&own_address).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(err.detail().contains("own mailbox"), "{err}");
    }
}

#[tokio::test]
async fn live_an_address_that_could_restructure_the_url_never_reaches_graph() {
    let Some((token, shared)) =
        gated("live_an_address_that_could_restructure_the_url_never_reaches_graph")
    else {
        return;
    };
    let own = provider(&token, MailboxPrincipal::Me);
    // Graph decodes an encoded `/` and re-parses the path, so these are refused before any
    // request — and none of them may ever come back as a mailbox.
    for hostile in [
        "../me".to_owned(),
        format!("{shared}/../../me"),
        format!("{shared}/mailFolders/sentitems"),
        "a@b.example\\me".to_owned(),
    ] {
        match own.resolve_shared_mailbox(&hostile).await {
            Ok(resolved) => panic!("{hostile:?} resolved to {resolved:?}"),
            Err(err) => assert!(err.detail().contains("invalid mailbox address"), "{err}"),
        }
    }
}

#[tokio::test]
async fn live_a_client_bound_to_the_shared_mailbox_reads_its_folders() {
    let Some((token, shared)) =
        gated("live_a_client_bound_to_the_shared_mailbox_reads_its_folders")
    else {
        return;
    };
    let handle = provider(&token, MailboxPrincipal::Me)
        .resolve_shared_mailbox(&shared)
        .await
        .expect("resolves")
        .handle;
    // Binding is the handle discovery returned, turned back into a principal.
    let bound = provider(&token, MailboxPrincipal::shared(&handle).unwrap());
    let SyncUpdate::Snapshot { objects, .. } = bound
        .sync_mailboxes(&account(), None)
        .await
        .expect("the shared mailbox's folder list")
        .update
    else {
        panic!("the folder list is a snapshot");
    };
    assert!(objects.iter().any(|f| f.role == Some(MailboxRole::Inbox)));
    // Graph grants Full Access per mailbox and reports no folder rights.
    assert!(objects.iter().all(|f| f.access == MailboxAccess::owner()));

    // The bound client's identity is the shared mailbox's own, read from the directory.
    let identity = bound.sender_identities(&account()).await.expect("identity");
    assert!(identity[0].address.email.eq_ignore_ascii_case(&shared));
}
