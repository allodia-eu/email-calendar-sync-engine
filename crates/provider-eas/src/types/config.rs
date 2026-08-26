// SPDX-License-Identifier: MPL-2.0
//! Connection/auth configuration and server-capability (OPTIONS) results.

use serde::{Deserialize, Serialize};

use crate::auth::EasAuth;
// ---------- Configuration ----------

/// Connection/auth configuration for one EAS account — what the host layer
/// builds from its stored account record and hands to `EasClient`.
#[derive(Clone, Serialize, Deserialize)]
pub struct EasConfig {
    /// Full URL to the Exchange ActiveSync endpoint, e.g.
    /// `https://mail.example.com/Microsoft-Server-ActiveSync`.
    pub url: String,
    /// Username for Basic auth. For domain accounts use `DOMAIN\user` or `user@domain`.
    pub username: String,
    /// EAS `User` query parameter — the mailbox's primary address per MS-ASHTTP.
    /// Empty → the client falls back to `username` (older configs, DOMAIN\user auth).
    #[serde(default)]
    pub user: String,
    /// Plaintext password (transported over TLS; encrypted at rest via crypto::encrypt).
    pub password: String,
    /// Protocol version: `"2.5"`, `"12.0"`, `"12.1"`, `"14.0"`, `"14.1"`, `"16.0"`, `"16.1"`.
    /// Default `"16.1"` for Exchange 2016/2019/Online.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: String,
    /// Device ID — alphanumeric, max 16 chars. Generated once per install by
    /// the host (persisted alongside its account secrets) and threaded in
    /// here; the crate itself never mints one.
    pub device_id: String,
    /// Device type — `"KylinsMail"` by convention. Sent in the X-MS-DeviceType header.
    #[serde(default = "default_device_type")]
    pub device_type: String,
    /// User-agent string. Defaults to `"KylinsMail/1.0"`.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Policy key returned by Provision command (MVP skips Provision, so this stays `"0"`).
    /// If the server demands provisioning, sync will return status 142; we surface that
    /// to the user as a "policy required" error.
    #[serde(default)]
    pub policy_key: String,
    /// Auth strategy selector. `"basic"` (default, historical) uses
    /// `username` / `password`. `"oauth"` means the source layer also fills
    /// `auth` with an `EasAuth::OAuth { .. }` built from the account's stored
    /// OAuth fields. Kept as a free-form `String` (not an enum) so the config
    /// round-trips through serde without a migration when new modes land.
    #[serde(default)]
    pub auth_type: String,
    /// Typed auth payload. Built by `EasSource::eas_config()` when
    /// `auth_type == "oauth"`; the transport calls
    /// `auth.authorization_header()` when `Some`, else falls back to Basic
    /// with `username` / `password`. `None` preserves the historical Basic
    /// path (existing tests construct `EasConfig { .. }` without it).
    #[serde(default)]
    pub auth: Option<EasAuth>,
}

fn default_protocol_version() -> String {
    "16.1".to_string()
}

fn default_device_type() -> String {
    "KylinsMail".to_string()
}

fn default_user_agent() -> String {
    "KylinsMail/1.0".to_string()
}

/// Manual `Default` so adding new optional fields (`auth_type`, `auth`, `user`)
/// doesn't force every construction site to name them. NOTE: the
/// `eas_source::eas_config` literal names every field explicitly (it does NOT
/// use `..Default::default()`), so new fields must be added there too —
/// otherwise the crate fails to compile. The `#[serde(default = "...")]`
/// attributes only cover deserialization, so without this impl,
/// `EasConfig { ..Default::default() }` wouldn't compile.
impl Default for EasConfig {
    fn default() -> Self {
        Self {
            url: String::default(),
            username: String::default(),
            user: String::default(),
            password: String::default(),
            protocol_version: default_protocol_version(),
            device_id: String::default(),
            device_type: default_device_type(),
            user_agent: default_user_agent(),
            policy_key: String::default(),
            auth_type: String::default(),
            auth: None,
        }
    }
}

/// Manual redacting `Debug` — the `EasAuth` precedent: `password` is plaintext
/// in memory and must NEVER render, not even in debug/panic inspection. `auth`
/// may render because `EasAuth`'s own `Debug` redacts its Basic password and
/// the OAuth token provider; the remaining fields carry no secrets.
impl std::fmt::Debug for EasConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EasConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("protocol_version", &self.protocol_version)
            .field("device_id", &self.device_id)
            .field("device_type", &self.device_type)
            .field("user_agent", &self.user_agent)
            .field("policy_key", &self.policy_key)
            .field("auth_type", &self.auth_type)
            .field("auth", &self.auth)
            .finish()
    }
}

impl EasConfig {
    /// The EAS `User` query param: `user` when set, else `username`.
    pub fn user_param(&self) -> &str {
        if self.user.is_empty() {
            &self.username
        } else {
            &self.user
        }
    }
}

// ---------- Options (server capabilities) ----------

/// Result of an HTTP OPTIONS round-trip against the EAS endpoint
/// ([MS-ASHTTP] §2.2.1.1): the server's advertised protocol versions and
/// supported command list. Used at account setup to negotiate the protocol
/// version (`client::pick_protocol_version`) before any WBXML command runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EasServerOptions {
    /// Entries of the `MS-ASProtocolVersions` header, comma-split and trimmed
    /// (e.g. `["2.5","12.0","12.1","14.0","14.1","16.0","16.1"]`). Empty when
    /// the header was absent.
    pub protocol_versions: Vec<String>,
    /// Entries of the `MS-ASProtocolCommands` header, comma-split and trimmed
    /// (e.g. `["Sync","SendMail","Provision", ...]`). Empty when the header
    /// was absent.
    pub commands: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_param_prefers_user_then_falls_back_to_username() {
        let mut cfg = EasConfig {
            username: "DOMAIN\\felix".into(),
            user: "felixzhou@example.org".into(),
            ..Default::default()
        };
        assert_eq!(cfg.user_param(), "felixzhou@example.org");
        cfg.user.clear();
        assert_eq!(cfg.user_param(), "DOMAIN\\felix");
    }

    #[test]
    fn config_without_user_field_deserializes_with_empty_user() {
        let json = r#"{"url":"https://x/Microsoft-Server-ActiveSync","username":"u","password":"p","device_id":"d"}"#;
        let cfg: EasConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(cfg.user, "");
        assert_eq!(cfg.user_param(), "u");
    }

    /// Manual-redaction guard (the `EasAuth::Debug` precedent): neither the
    /// config's own `password` nor an embedded Basic `EasAuth` password may
    /// ever reach a Debug render — debug/panic output is log-adjacent.
    #[test]
    fn debug_redacts_config_and_auth_passwords() {
        let cfg = EasConfig {
            username: "alice".into(),
            password: "s3cret-config".into(),
            auth: Some(EasAuth::basic("alice", "s3cret-auth")),
            ..Default::default()
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("s3cret-config"),
            "config password must be redacted: {dbg}"
        );
        assert!(
            !dbg.contains("s3cret-auth"),
            "EasAuth password must be redacted: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
        // Non-secret fields stay visible so the render stays useful.
        assert!(dbg.contains("alice"));
    }
}
