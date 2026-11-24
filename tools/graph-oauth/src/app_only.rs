//! `app-token` — an **app-only** access token (OAuth 2.0 client credentials, RFC 6749 §4.4), for
//! the live tests of what a host holding application permissions reads: a transfer host reading
//! another user's mailbox settings, or a shared mailbox's, where a delegated token is refused.
//!
//! The tenant id, client id and client secret come from a JSON file
//! (`{"tenant_id", "client_id", "client_secret"}`), by default `.local/app-only.json` beside the
//! tokens (gitignored), or the path in `GRAPH_APP_CONFIG`. The secret is never printed; the token
//! is printed alone, so a live test can capture it in a `$(...)`. Nothing is cached: a client
//! credentials token is one request away.

use serde_json::Value;

use super::{Res, manifest_dir, post_token, str_field};

/// The scope that asks for every application permission the app was granted.
const DEFAULT_SCOPE: &str = "https://graph.microsoft.com/.default";

pub(super) fn cmd_app_token() -> Res<()> {
    let path = std::env::var("GRAPH_APP_CONFIG")
        .unwrap_or_else(|_| format!("{}/.local/app-only.json", manifest_dir()));
    let bytes = std::fs::read(&path).map_err(|e| format!("no app config at {path} ({e})"))?;
    let config: Value = serde_json::from_slice(&bytes)?;
    let tenant = str_field(&config, "tenant_id")?;
    let client_id = str_field(&config, "client_id")?;
    let secret = str_field(&config, "client_secret")?;
    let authority = format!("https://login.microsoftonline.com/{tenant}");
    let resp = post_token(
        &authority,
        &[
            ("client_id", client_id.as_str()),
            ("client_secret", secret.as_str()),
            ("scope", DEFAULT_SCOPE),
            ("grant_type", "client_credentials"),
        ],
    )?;
    println!("{}", str_field(&resp, "access_token")?);
    Ok(())
}
