// SPDX-License-Identifier: MPL-2.0
//! Gated live tests against a real Exchange/O365 account.
//!
//! These tests are `#[ignore]`d by default and additionally no-op when the
//! environment gates are unset, so a plain `cargo test -p provider-eas` never
//! touches the network. To run them for real:
//!
//! ```sh
//! # KYLINS_EAS_LIVE_USERNAME — optional: Basic-auth identity when it differs
//! # from the mailbox address (EAS `User` param); unset → identity = USER.
//! # KYLINS_EAS_LIVE_INSECURE — optional: set to 1 to accept self-signed certs.
//! KYLINS_EAS_LIVE_URL=https://mail.example.com/Microsoft-Server-ActiveSync \
//! KYLINS_EAS_LIVE_USER=user@example.com \
//! KYLINS_EAS_LIVE_PASS=app-password \
//! KYLINS_EAS_LIVE_USERNAME=user@example.local \
//! KYLINS_EAS_LIVE_INSECURE=1 \
//! cargo test -p provider-eas --test live_eas -- --include-ignored --nocapture
//! ```
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
    let url = std::env::var("KYLINS_EAS_LIVE_URL").ok()?;
    let user = std::env::var("KYLINS_EAS_LIVE_USER").ok()?;
    let pass = std::env::var("KYLINS_EAS_LIVE_PASS").ok()?;
    // Basic-auth identity. Distinct from the EAS `User` param when the
    // server's auth realm differs from the mailbox address (the on-prem
    // test server: auth felixzhou@kylins.local, User felixzhou@example.org).
    // Unset → identity equals User (the common case).
    let username = std::env::var("KYLINS_EAS_LIVE_USERNAME").unwrap_or_else(|_| user.clone());
    // Self-signed test servers (e.g. the on-prem Exchange at
    // mail.example.org): opt in explicitly — default stays secure.
    let insecure = std::env::var("KYLINS_EAS_LIVE_INSECURE").is_ok();
    Some(EasConfig {
        url,
        username,
        user,
        password: pass,
        // Alphanumeric, <= 16 chars per MS-ASHTTP DeviceId constraints.
        device_id: "KYLINSLIVETEST01".to_string(),
        accept_invalid_certs: insecure,
        ..Default::default()
    })
}
