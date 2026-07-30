//! Where a profile's tokens live on disk, and how the record is built.
//!
//! Split out of `main.rs` (which keeps the command dispatch) so each file stays under
//! the repo's 500-line limit.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::Res;

/// Builds the on-disk token record, preserving config so `refresh`/`get` need no
/// re-passing. The refresh token rotates on each refresh, so it is always re-saved.
pub(crate) fn build_tokens(
    resp: &Value,
    client_id: &str,
    authority: &str,
    scopes: &str,
    profile: Option<&str>,
) -> Res<Value> {
    let access = resp
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("token response had no access_token")?;
    // On refresh, Microsoft may omit a new refresh_token; fall back to the old one.
    let refresh = resp
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            load_tokens(profile)
                .ok()
                .and_then(|t| str_field(&t, "refresh_token").ok())
        })
        .ok_or("no refresh_token in response or on disk")?;
    Ok(json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": resp.get("expires_in").and_then(Value::as_u64).unwrap_or(3600),
        "obtained_at": now_epoch(),
        "scope": resp.get("scope").and_then(Value::as_str).unwrap_or(scopes),
        "client_id": client_id,
        "authority": authority,
    }))
}

/// Where a profile's tokens live. An explicit `GRAPH_TOKENS` wins; otherwise the
/// unnamed profile keeps the original `tokens.json` (so an existing sign-in is
/// untouched) and a named one gets its own `tokens-<profile>.json` beside it.
pub(crate) fn tokens_path(profile: Option<&str>) -> String {
    if let Ok(path) = std::env::var("GRAPH_TOKENS") {
        return path;
    }
    let dir = env!("CARGO_MANIFEST_DIR");
    match profile {
        Some(name) => format!("{dir}/.local/tokens-{name}.json"),
        None => format!("{dir}/.local/tokens.json"),
    }
}

pub(crate) fn load_tokens(profile: Option<&str>) -> Res<Value> {
    let path = tokens_path(profile);
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("no tokens at {path} ({e}); run `login` first"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn save_tokens(tokens: &Value, profile: Option<&str>) -> Res<()> {
    let path = tokens_path(profile);
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(tokens)?)?;
    // The refresh token is a long-lived credential; keep it owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub(crate) fn str_field(v: &Value, key: &str) -> Res<String> {
    Ok(v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("tokens file missing `{key}`"))?
        .to_owned())
}

pub(crate) fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
