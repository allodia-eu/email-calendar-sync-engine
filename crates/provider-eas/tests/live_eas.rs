// SPDX-License-Identifier: MPL-2.0
//! Gated live tests against the shared O365/Exchange test account, same
//! skip-when-unset convention as
//! the provider-google/graph live suites (`GOOGLE_ACCESS_TOKEN` /
//! `GRAPH_ACCESS_TOKEN`), with the gates named `EAS_LIVE_*`.
//!
//! Gating is two-layered: every test carries `#[ignore = "live Exchange
//! account required"]` (a plain `cargo test -p provider-eas` reports them as
//! ignored and never touches the network) AND no-ops when the environment
//! gates are unset (an explicit `--include-ignored` run without credentials
//! prints one "live gates unset" line per test and exits 0 — a skip, not a
//! failure). Any required gate unset ⇒ the whole suite skips. To run for
//! real:
//!
//! ```sh
//! # Required gates — what each covers:
//! #   EAS_LIVE_URL      — the Microsoft-Server-ActiveSync endpoint.
//! #   EAS_LIVE_USER     — mailbox address (the EAS `User` query param).
//! #   EAS_LIVE_PASSWORD — the account password / app password.
//! # Optional gates:
//! #   EAS_LIVE_USERNAME — Basic-auth identity when it differs from the
//! #                       mailbox address (EAS `User` param); unset →
//! #                       identity = USER.
//! #   EAS_LIVE_INSECURE — set to 1 to trust the self-signed test server
//! #                       (test-builds-only accept-any TLS config;
//! #                       production builds never ship this path — trust
//! #                       comes from the host's `TlsPolicy`).
//! EAS_LIVE_URL=https://mail.example.com/Microsoft-Server-ActiveSync \
//! EAS_LIVE_USER=user@example.com \
//! EAS_LIVE_PASSWORD=app-password \
//! EAS_LIVE_USERNAME=user@example.local \
//! EAS_LIVE_INSECURE=1 \
//! cargo test -p provider-eas --test live_eas -- --include-ignored --nocapture
//! ```
//!
//! Observed transcripts from these runs must be captured as scrubbed fixtures
//! per the repo's rules (AGENTS.md "Identifiers in fixtures and docs use
//! reserved names"; see `docs/agent-guidance/eas.md` → "Live testing").
//!
//! The scaffold uses the Basic-auth path (`EasConfig { username, password, ..
//! }` with `auth: None`). OAuth accounts need a token provider wired through
//! `types::EasConfig::auth` and are out of scope for this smoke test.

use provider_eas::{
    client::{EasClient, pick_protocol_version},
    types::{EasConfig, SyncRequest},
};

#[path = "live_eas/calendar_folder_probes.rs"]
mod calendar_folder_probes;
#[path = "live_eas/calendar_item_probe.rs"]
mod calendar_item_probe;
#[path = "live_eas/calendar_smoke.rs"]
mod calendar_smoke;
#[path = "live_eas/contacts_smoke.rs"]
mod contacts_smoke;
#[path = "live_eas/provision_smoke.rs"]
mod provision_smoke;
#[path = "live_eas/sync_smoke.rs"]
mod sync_smoke;

/// Protocol versions this client knows how to speak, mirroring the account
/// setup flow in the host app (kylins `api::eas`).
const CLIENT_KNOWN_VERSIONS: &[&str] = &["2.5", "12.0", "12.1", "14.0", "14.1", "16.0", "16.1"];

/// Build an `EasConfig` from the environment gates, or `None` when any gate
/// is unset (the test then returns immediately — a skip, not a pass/fail).
fn live_config() -> Option<EasConfig> {
    let url = std::env::var("EAS_LIVE_URL").ok()?;
    let user = std::env::var("EAS_LIVE_USER").ok()?;
    let pass = std::env::var("EAS_LIVE_PASSWORD").ok()?;
    // Basic-auth identity. Distinct from the EAS `User` param when the
    // server's auth realm differs from the mailbox address (the on-prem
    // test server: auth felixzhou@kylins.local, User felixzhou@example.org).
    // Unset → identity equals User (the common case).
    let username = std::env::var("EAS_LIVE_USERNAME").unwrap_or_else(|_| user.clone());
    Some(EasConfig {
        url,
        username,
        user,
        password: pass,
        // Alphanumeric, <= 16 chars per MS-ASHTTP DeviceId constraints.
        device_id: "KYLINSLIVETEST01".to_string(),
        ..Default::default()
    })
}

/// The harness TLS config. Verifying bundled Mozilla roots by default; the
/// `EAS_LIVE_INSECURE` gate swaps in the test-builds-only accept-any
/// config for the self-signed on-prem test server. (The production
/// `accept_invalid_certs` escape is gone — trust is the host's `TlsPolicy`,
/// realized per account via `engine_tls::client_config`; see
/// `docs/agent-guidance/eas.md`. The `dangerous-testing` feature exists only
/// in this crate's dev builds.)
fn live_tls() -> engine_tls::TlsClientConfig {
    if std::env::var("EAS_LIVE_INSECURE").is_ok() {
        engine_tls::TlsClientConfig::dangerous_accept_any()
    } else {
        engine_tls::TlsClientConfig::bundled()
    }
}

/// Build a live `EasClient` on the harness TLS config — every probe/smoke
/// test's one construction path, so none can drift off the shared policy.
fn live_client(config: EasConfig) -> EasClient {
    EasClient::new(config, &live_tls()).expect("live EAS client build")
}
