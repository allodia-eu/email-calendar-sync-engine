//! Gated live check that [`GoogleClient::affiliation`] tells a personal Google account from a
//! Workspace one, against Google rather than a fixture, with a token holding `userinfo.email`
//! alone: the one identity scope a host can count on.
//!
//! Both arms run in one invocation, because the claim is the difference: a client that answered
//! `Personal` to everything passes the personal arm alone. Each arm skips without its token.
//!
//! ```sh
//! # Two accounts signed in with tools/google-oauth, one personal and one Workspace:
//! #   cargo run --manifest-path tools/google-oauth/Cargo.toml -- login --client-id <ID> ...
//! #   GOOGLE_TOKENS=tools/google-oauth/.local/tokens-workspace.json cargo run ... -- login ...
//! # then mint an access token from each, narrowed to userinfo.email (`scope=` on the refresh),
//! # and pass them as GOOGLE_PERSONAL_ACCESS_TOKEN and GOOGLE_WORKSPACE_ACCESS_TOKEN.
//! cargo test -p provider-google --test live_affiliation -- --nocapture
//! ```

use engine_core::affiliation::Affiliation;
use engine_http::RetryConfig;
use engine_tls::TlsClientConfig;
use provider_google::GoogleClient;

fn client(variable: &str) -> Option<GoogleClient> {
    let token = std::env::var(variable)
        .ok()
        .filter(|token| !token.is_empty())?;
    Some(
        GoogleClient::connect(token, &TlsClientConfig::bundled(), &RetryConfig::default())
            .expect("a Google client"),
    )
}

#[tokio::test]
async fn a_personal_account_and_a_workspace_account_are_told_apart() {
    let personal = client("GOOGLE_PERSONAL_ACCESS_TOKEN");
    let workspace = client("GOOGLE_WORKSPACE_ACCESS_TOKEN");
    if personal.is_none() && workspace.is_none() {
        eprintln!("skipping: set GOOGLE_PERSONAL_ACCESS_TOKEN and GOOGLE_WORKSPACE_ACCESS_TOKEN");
        return;
    }
    if let Some(client) = personal {
        let affiliation = client
            .affiliation()
            .await
            .expect("a personal account's answer");
        // The answer is never printed: a hosted domain identifies an organization.
        assert!(
            affiliation == Affiliation::Personal,
            "a personal account read as an organization"
        );
        eprintln!("personal account: personal");
    } else {
        eprintln!("!! NOT VERIFIED: the personal arm (GOOGLE_PERSONAL_ACCESS_TOKEN unset)");
    }
    if let Some(client) = workspace {
        let affiliation = client
            .affiliation()
            .await
            .expect("a Workspace account's answer");
        assert!(
            matches!(affiliation, Affiliation::Organization(_)),
            "a Workspace account read as personal"
        );
        eprintln!("Workspace account: an organization");
    } else {
        eprintln!("!! NOT VERIFIED: the Workspace arm (GOOGLE_WORKSPACE_ACCESS_TOKEN unset)");
    }
}
