//! Gated live integration: reading and renaming the account's sender identity against
//! the Stalwart harness.
//!
//! The offline suite cannot prove any of this. The fake executor answers canned bytes
//! whatever it is sent, so a `Identity/set` naming the wrong property, the wrong
//! capability URN or the wrong account id passes there and fails here.
//!
//! Both directions are asserted rather than only the write. Recording "the name came
//! back" alone would pass against an adapter that sent nothing and a server that
//! already held that name, so each rename is read back **and** followed by its inverse,
//! and the *change* is what is pinned. That also leaves the harness account as it was
//! found, which matters because the suites share it.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use engine_core::ids::AccountId;
use engine_provider::{IdentityControls, Provider, SenderIdentity};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect")
}

/// The identity for the harness account's own address.
async fn own_identity(provider: &JmapProvider) -> SenderIdentity {
    let identities = provider.sender_identities(&account()).await.expect("read");
    assert!(
        !identities.is_empty(),
        "a submission account has at least one identity"
    );
    identities
        .into_iter()
        .find(|identity| {
            identity
                .address
                .email
                .eq_ignore_ascii_case("alice@test.local")
        })
        .expect("an identity for the harness account's own address")
}

#[tokio::test]
async fn the_session_advertises_identities_and_the_server_returns_the_accounts_own_address() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live identity test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    // The capability rides the submission URN, so a server that advertises submission
    // must answer `Identity/get`. If it did not, the capability would be a claim the
    // adapter cannot back.
    assert_eq!(
        provider.connection_info().capabilities.sender_identities(),
        Some(IdentityControls::Writable)
    );

    let identity = own_identity(&provider).await;
    assert!(
        !identity.id.as_str().is_empty(),
        "the handle is what a rename targets"
    );
}

#[tokio::test]
async fn a_rename_reaches_the_server_and_the_inverse_puts_it_back() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live identity rename test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    let before = own_identity(&provider).await;
    let original = before.address.name.clone();
    // A value nothing else could have left behind, so a read that returns it is
    // evidence of *this* write rather than of the seed.
    let probe = "Live Identity Probe";
    assert_ne!(original.as_deref(), Some(probe));

    provider
        .set_sender_name(&account(), &before.id, probe)
        .await
        .expect("rename");

    let renamed = own_identity(&provider).await;
    assert_eq!(
        renamed.address.name.as_deref(),
        Some(probe),
        "the server stored what we sent"
    );
    assert_eq!(
        renamed.address.email, before.address.email,
        "a rename changes the name and not the address"
    );

    // Put it back, and assert the *restore* landed too: an adapter whose write silently
    // did nothing would fail here rather than leaving a poisoned account behind.
    provider
        .set_sender_name(&account(), &before.id, original.as_deref().unwrap_or(""))
        .await
        .expect("restore");
    let restored = own_identity(&provider).await;
    assert_eq!(restored.address.name, original);
}
