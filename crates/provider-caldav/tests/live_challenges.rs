//! Gated live integration: a connect through each way a real server asks for the credential,
//! against the **SabreDAV** harness (`docker/sabredav/`), which serves the same account three
//! ways:
//!
//! - `/`: a `401` with a Basic challenge (Stalwart, iCloud and Fastmail answer this way);
//! - `/open/`: a `207` that lets the anonymous request through and names the requester
//!   `<DAV:unauthenticated/>` (RFC 5397 §3; a SabreDAV deployment that allows anonymous access);
//! - `/bare/`: a `401` with no `WWW-Authenticate` at all (Yahoo, Google and AOL answer this way).
//!
//! Each must end connected as the account: its principal found, its calendars listed, its
//! address set read. `src/challenge_tests.rs` pins the same rules offline. Skips unless
//! `SABREDAV_HTTP_ADDR` is set, so the offline `cargo test --workspace` stays green.

use core::time::Duration;

use engine_core::ids::AccountId;
use engine_provider::{CalendarUserAddresses, CalendarWrites, Provider};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};

/// Reads the SabreDAV harness coordinates, or `None` to skip (offline gate).
fn harness() -> Option<(String, String, String)> {
    let addr = std::env::var("SABREDAV_HTTP_ADDR").ok()?;
    let user = std::env::var("SABREDAV_USER").unwrap_or_else(|_| "alice@test.local".to_owned());
    let pass = std::env::var("SABREDAV_PASS").unwrap_or_else(|_| "sabredav-alice-pw".to_owned());
    Some((addr, user, pass))
}

/// Connects starting at `start`, retrying briefly so a just-started container is tolerated.
async fn connect(addr: &str, user: &str, pass: &str, start: &str) -> CalDavProvider {
    let config = CalDavConfig::new(
        format!("http://{addr}"),
        Credentials::Basic {
            username: user.to_owned(),
            password: pass.to_owned(),
        },
    )
    .with_discovery_path(start);
    let mut last_err = None;
    for _ in 0..15 {
        match CalDavProvider::connect(config.clone()).await {
            Ok(provider) => return provider,
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect through {start}: {last_err:?}");
}

/// Connects through `prefix` and checks the connection is the account's.
async fn connects_as_the_account(prefix: &str) {
    let Some((addr, user, pass)) = harness() else {
        eprintln!("skipping the {prefix} challenge test: SABREDAV_HTTP_ADDR unset");
        return;
    };
    let provider = connect(&addr, &user, &pass, prefix).await;
    let account = AccountId::try_from("sabredav-challenge-live").unwrap();

    assert_eq!(
        provider.principal_url(),
        Some(format!("http://{addr}{prefix}principals/{user}/").as_str()),
        "the principal the server names once it knows who asks"
    );
    let calendars = provider
        .sync_calendars(&account, None)
        .await
        .expect("the home is listed as the account");
    let engine_core::sync::SyncUpdate::Snapshot { objects, .. } = calendars.update else {
        panic!("a calendar listing is a snapshot");
    };
    assert!(!objects.is_empty(), "the account's calendars");
    assert_eq!(
        provider
            .calendar_user_addresses(&account)
            .await
            .expect("the principal answers"),
        CalendarUserAddresses::Known(vec![user])
    );
}

#[tokio::test]
async fn a_server_that_challenges_is_answered() {
    connects_as_the_account("/").await;
}

#[tokio::test]
async fn a_server_that_lets_an_anonymous_request_through_is_answered() {
    connects_as_the_account("/open/").await;
}

#[tokio::test]
async fn a_server_that_answers_401_without_a_challenge_is_answered() {
    connects_as_the_account("/bare/").await;
}
