//! Gated live checks for the account's send-as identities against a real Google account.
//!
//! Three things only a live call can settle, and each of them decides something in the host:
//!
//! - **Which scope the settings endpoint really wants.** `users.settings.sendAs` is documented
//!   under `gmail.settings.basic`, not under the `https://mail.google.com/` scope the rest of
//!   this adapter runs on. If the mail scope turns out to be enough, no host has to ask its
//!   users to consent again; if it is not, every existing Google grant needs re-consenting.
//!   That is the whole reason this file exists, and the read below is what answers it.
//! - **That a percent-encoded `@` is accepted in the path segment.** The send-as resource is keyed
//!   by the address itself, and an address may legally carry characters that would reshape a raw
//!   path, so the adapter encodes it. No offline fake can say whether Google accepts that encoding.
//! - **That `displayName` alone is a valid patch**, and that Gmail stores what it is sent.
//!
//! The rename is followed by its inverse, and the restore is asserted too: recording only that
//! the name came back would pass against an adapter that sent nothing and an account that
//! already held that name. It also leaves the shared throwaway account as it was found.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set. Run single-threaded: one test renames the
//! account the other reads. The rename additionally skips, loudly, on a token that carries
//! only the mail scope, since that is an environmental fact rather than a defect.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_identity -- --nocapture --test-threads=1
//! ```

use engine_core::ids::AccountId;
use engine_provider::{IdentityControls, Provider, SenderIdentity};
use provider_google::{GmailProvider, GoogleClient};

/// The test account's own address, which is also its primary send-as entry.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
}

/// The send-as entry for the account's own address.
async fn own_identity(provider: &GmailProvider) -> SenderIdentity {
    let identities = provider
        .sender_identities(&account())
        .await
        .expect("reading send-as settings");
    identities
        .into_iter()
        .find(|identity| identity.address.email.eq_ignore_ascii_case(SELF_ADDRESS))
        .expect("a send-as entry for the account's own address")
}

#[tokio::test]
async fn the_send_as_settings_are_readable_with_this_token() {
    let Some(token) = token() else {
        eprintln!("skipping live send-as read: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // The capability the host reads before drawing an editor. Gmail's names belong to the
    // account holder, so it claims the write; whether *this* token may reach the settings API
    // is the question the call below answers, and no capability can.
    assert_eq!(
        provider.connection_info().capabilities.sender_identities(),
        Some(IdentityControls::Writable)
    );

    let identity = own_identity(&provider).await;
    // The handle is the address, because that is how the settings resource is keyed: a rename
    // puts it in the path.
    assert_eq!(identity.id.as_str(), identity.address.email);
}

#[tokio::test]
async fn a_rename_reaches_gmail_and_the_inverse_puts_it_back() {
    let Some(token) = token() else {
        eprintln!("skipping live send-as rename: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    let before = own_identity(&provider).await;
    let original = before.address.name.clone();
    // A value nothing else could have left behind, so reading it back is evidence of *this*
    // write rather than of whatever the account already held.
    let probe = "Live Identity Probe";
    assert_ne!(original.as_deref(), Some(probe));

    match provider
        .set_sender_name(&account(), &before.id, probe)
        .await
    {
        Ok(()) => {}
        // The one environmental outcome that is not a defect: `sendAs.patch` needs
        // `gmail.settings.basic`, which the read above does **not**. Skipping here rather than
        // failing keeps a mail-only token green, and the moment somebody re-consents with the
        // settings scope this test starts running for real, with nothing to remember. Matching
        // the reason string is deliberate: Google returns a plain 403 for several unrelated
        // causes, and only this one may be skipped.
        Err(err) if err.to_string().contains("ACCESS_TOKEN_SCOPE_INSUFFICIENT") => {
            eprintln!(
                "skipping the send-as write: this token lacks \
                 https://www.googleapis.com/auth/gmail.settings.basic. Re-run \
                 `google-oauth login` with it added to --scopes to exercise the write."
            );
            return;
        }
        Err(err) => panic!("renaming the send-as entry: {err}"),
    }

    let renamed = own_identity(&provider).await;
    assert_eq!(
        renamed.address.name.as_deref(),
        Some(probe),
        "Gmail stored what we sent"
    );
    assert_eq!(
        renamed.address.email, before.address.email,
        "a rename changes the name and not the address"
    );

    // Put it back, and assert the restore landed: an adapter whose write silently did nothing
    // would fail here rather than leaving a poisoned shared account behind.
    provider
        .set_sender_name(&account(), &before.id, original.as_deref().unwrap_or(""))
        .await
        .expect("restoring the send-as entry");
    assert_eq!(own_identity(&provider).await.address.name, original);
}
