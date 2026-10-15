//! Gated live check that [`GraphClient::affiliation`] tells a personal Microsoft account from a
//! work or school one, against Microsoft rather than a fixture.
//!
//! Both arms run in one invocation, because the claim is the difference: a client that answered
//! `Personal` to everything, or `Organization` to everything, passes either arm alone. Each arm
//! skips without its token.
//!
//! ```sh
//! T=tools/graph-oauth/.local
//! token() { python3 -c "import json;print(json.load(open('$1'))['access_token'])"; }
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh --profile m365
//! GRAPH_PERSONAL_ACCESS_TOKEN="$(token $T/tokens.json)" \
//! GRAPH_WORK_ACCESS_TOKEN="$(token $T/tokens-m365.json)" \
//!   cargo test -p provider-graph --test live_affiliation -- --nocapture
//! ```

use engine_core::affiliation::Affiliation;
use engine_http::RetryConfig;
use engine_tls::TlsClientConfig;
use provider_graph::GraphClient;

fn client(variable: &str) -> Option<GraphClient> {
    let token = std::env::var(variable)
        .ok()
        .filter(|token| !token.is_empty())?;
    Some(
        GraphClient::connect(token, &TlsClientConfig::bundled(), &RetryConfig::default())
            .expect("a Graph client"),
    )
}

#[tokio::test]
async fn a_personal_account_and_a_work_account_are_told_apart() {
    let personal = client("GRAPH_PERSONAL_ACCESS_TOKEN");
    let work = client("GRAPH_WORK_ACCESS_TOKEN");
    if personal.is_none() && work.is_none() {
        eprintln!("skipping: set GRAPH_PERSONAL_ACCESS_TOKEN and GRAPH_WORK_ACCESS_TOKEN");
        return;
    }
    if let Some(client) = personal {
        let affiliation = client
            .affiliation()
            .await
            .expect("a personal account's answer");
        // The answer is never printed: an organization's tenant id identifies it.
        assert!(
            affiliation == Affiliation::Personal,
            "a personal account read as an organization"
        );
        eprintln!("personal account: personal");
    } else {
        eprintln!("!! NOT VERIFIED: the personal arm (GRAPH_PERSONAL_ACCESS_TOKEN unset)");
    }
    if let Some(client) = work {
        let affiliation = client.affiliation().await.expect("a work account's answer");
        assert!(
            matches!(&affiliation, Affiliation::Organization(id) if !id.as_str().is_empty()),
            "a work account read as personal"
        );
        eprintln!("work account: an organization");
    } else {
        eprintln!("!! NOT VERIFIED: the work arm (GRAPH_WORK_ACCESS_TOKEN unset)");
    }
}
