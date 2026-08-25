// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use super::{EasClient, EasError};

impl EasClient {
    /// The hierarchy sync key for folder ops — "0" before the first FolderSync.
    pub fn hierarchy_key(&self) -> &str {
        if self.hierarchy_sync_key.is_empty() {
            "0"
        } else {
            &self.hierarchy_sync_key
        }
    }

    /// Read-only access to the in-memory cached hierarchy sync key ("0"
    /// before the first successful FolderSync, via the same fallback as
    /// `hierarchy_key()`). `EasSource::list_folders` persists this after a
    /// successful FolderSync round so the next round resumes with the
    /// server-issued key instead of re-bootstrapping from "0".
    pub fn hierarchy_sync_key_str(&self) -> &str {
        self.hierarchy_key()
    }

    /// Prime the in-memory hierarchy sync key from the persisted one
    /// (`accounts.eas_hierarchy_key`) so a folder op (FolderCreate/Update/
    /// Delete) can go out WITHOUT a preceding FolderSync round — the op
    /// requests carry the key per MS-ASCMD, and a fresh client would
    /// otherwise send the bootstrap key "0" (stale-key status in reply).
    /// Cheaper than issuing a FolderSync just to warm the cache. An empty
    /// string is ignored (same "0" fallback as `hierarchy_key()`).
    pub fn set_hierarchy_sync_key(&mut self, key: String) {
        if !key.is_empty() {
            self.hierarchy_sync_key = key;
        }
    }

    /// Read-only access to the current policy key. The retry layer's
    /// `RunProvision` branch rotates this in place via `provision()`; the
    /// source layer reads it after a successful round to persist the rotated
    /// key for the next sync. Avoids leaking the full `EasConfig` (which
    /// carries secrets).
    pub fn policy_key(&self) -> &str {
        &self.config.policy_key
    }

    /// Read-only access to the redirect endpoint adopted during this
    /// client's command rounds (HTTP 451 `X-MS-Location` follow), or `None`
    /// when no redirect was adopted. The source layer reads it after a
    /// successful round to persist the adopted URL (`accounts.eas_url`),
    /// mirroring the `policy_key()` persistence. Avoids leaking the full
    /// `EasConfig` (which carries secrets).
    pub fn adopted_url(&self) -> Option<&str> {
        self.adopted_url.as_deref()
    }

    /// Adopt an HTTP 451 `X-MS-Location` redirect target ([MS-ASHTTP]
    /// §2.2.1.1.2.4 / §3.1.5.2): validate the location via
    /// `endpoint_from_x_ms_location`, switch this client's base URL to the
    /// derived endpoint, and record the adopted URL for the source layer to
    /// persist. Logs the hop (from → to) at info. An invalid location is
    /// logged at warn and surfaced as an error — the old URL stays untouched
    /// and nothing is recorded.
    ///
    /// `pub` (not `pub(crate)`) because the host's source layer exercises it
    /// directly in redirect-persistence tests (kylins'
    /// `sync::eas_source::persist_eas_url_writes_adopted_url_against_current_row`).
    ///
    /// # Errors
    ///
    /// Returns `EasError` when `location` is not a usable absolute EAS endpoint;
    /// the client keeps its previous URL in that case.
    pub fn adopt_redirect_location(&mut self, location: &str) -> Result<(), EasError> {
        let new_url = endpoint_from_x_ms_location(location).map_err(|e| {
            log::warn!("EAS HTTP 451 redirect not followed: {e}");
            e
        })?;
        let old_url = std::mem::replace(&mut self.config.url, new_url.clone());
        self.adopted_url = Some(new_url.clone());
        log::info!(
            "EAS HTTP 451 redirect hop: {old_url} → {new_url} — retrying against the new server"
        );
        Ok(())
    }
}

/// The fixed EAS endpoint path ([MS-ASHTTP] §2.1). Redirect adoption derives
/// the new endpoint as `https://<location authority>` + this path — the
/// location's own path/query/fragment never carries over (so a location that
/// already ends in the EAS path is never doubled).
const EAS_ENDPOINT_PATH: &str = "/Microsoft-Server-ActiveSync";

/// Validate an HTTP 451 `X-MS-Location` header value ([MS-ASHTTP]
/// §2.2.1.1.2.4) and derive the EAS endpoint URL to adopt:
///   * must be an ABSOLUTE `https://` URL — an `http://` location is a plaintext downgrade and
///     anything else (relative, other scheme, garbage) is untrusted; both reject with a descriptive
///     error, never silently;
///   * must carry a host;
///   * the authority must carry NO userinfo (`user[:password]@host`): the server-controlled,
///     credential-shaped string would otherwise be persisted verbatim into the plaintext
///     `accounts.eas_url` column and the hop log. Rejected with a descriptive error — never
///     silently stripped and adopted — matching the refuse-downgrade posture; the userinfo value
///     itself is never logged (only the host is);
///   * the new endpoint is `https://` + the location's authority (host and port, case preserved) +
///     the fixed [`EAS_ENDPOINT_PATH`] — any path/query/fragment the location carries is dropped,
///     so a location already ending in the EAS path is never doubled and any query string is
///     stripped.
///
/// Scheme match is case-insensitive per RFC 3986 (`HTTPS://` is accepted).
/// No network I/O — unit-tested without a live server (a rejection only
/// emits a redacted warn log).
fn endpoint_from_x_ms_location(location: &str) -> Result<String, EasError> {
    const HTTPS_SCHEME: &str = "https://";
    let trimmed = location.trim();
    let lower = trimmed.to_ascii_lowercase();
    let after_scheme = if lower.starts_with(HTTPS_SCHEME) {
        // Re-slice the ORIGINAL value at the same byte offset — ASCII case
        // folding preserves length, and the authority's case is kept as the
        // server sent it.
        &trimmed[HTTPS_SCHEME.len()..]
    } else if lower.starts_with("http://") {
        return Err(EasError::Transport(format!(
            "X-MS-Location rejected: '{trimmed}' is a plaintext http:// URL — refusing to downgrade from https"
        )));
    } else {
        return Err(EasError::Transport(format!(
            "X-MS-Location rejected: '{trimmed}' is not an absolute https:// URL — refusing to follow"
        )));
    };
    // The authority ends at the first '/', '?' or '#' (path/query/fragment).
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.is_empty() {
        return Err(EasError::Transport(format!(
            "X-MS-Location rejected: '{trimmed}' carries no host — refusing to follow"
        )));
    }
    // The authority must carry NO userinfo (`user[:password]@host`): the
    // server-controlled, credential-shaped string would otherwise land
    // verbatim in the hop log and the plaintext `accounts.eas_url` column.
    // Reject — never strip-and-adopt — matching the refuse-downgrade
    // posture. Per RFC 3986 the LAST '@' delimits userinfo from host, so
    // the remainder is the (non-credential) host[:port] — safe to log; the
    // userinfo value itself never goes into the log or the error.
    if authority.contains('@') {
        let host = authority.rsplit('@').next().unwrap_or_default();
        log::warn!(
            "EAS HTTP 451 redirect rejected: X-MS-Location authority carries userinfo (redirect host: {host}) — refusing to follow; the credential-shaped userinfo is deliberately not logged or persisted"
        );
        return Err(EasError::Transport(
            "X-MS-Location rejected: the authority carries userinfo (a credential-shaped prefix before the host) — refusing to follow; the userinfo value is deliberately not logged or persisted"
                .to_string(),
        ));
    }
    Ok(format!("https://{authority}{EAS_ENDPOINT_PATH}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EasConfig;

    #[test]
    fn hierarchy_key_falls_back_to_zero_before_first_folder_sync() {
        let client = EasClient::new(EasConfig::default());
        assert_eq!(client.hierarchy_key(), "0");
    }

    /// Phase B Task 2: the public getter surfaces the in-memory cached key so
    /// `EasSource::list_folders` can persist it after a successful FolderSync.
    /// Pre-FolderSync it yields the same "0" fallback as `hierarchy_key()`.
    #[test]
    fn hierarchy_sync_key_str_returns_cached_key_or_zero_fallback() {
        let mut client = EasClient::new(EasConfig::default());
        assert_eq!(
            client.hierarchy_sync_key_str(),
            "0",
            "empty cache must surface the \"0\" bootstrap fallback"
        );
        client.hierarchy_sync_key = "hier-9".to_string(); // as if FolderSync ran
        assert_eq!(client.hierarchy_sync_key_str(), "hier-9");
    }

    /// Phase B Task 7: `set_hierarchy_sync_key` primes the cache from the
    /// persisted key so a folder op can go out without a preceding FolderSync;
    /// an empty string is ignored (the "0" bootstrap fallback must survive).
    #[test]
    fn set_hierarchy_sync_key_primes_cache_and_ignores_empty() {
        let mut client = EasClient::new(EasConfig::default());
        client.set_hierarchy_sync_key(String::new());
        assert_eq!(client.hierarchy_key(), "0", "empty prime must be a no-op");
        client.set_hierarchy_sync_key("hier-7".to_string());
        assert_eq!(client.hierarchy_key(), "hier-7");
    }

    #[test]
    fn x_ms_location_https_location_yields_fixed_eas_endpoint() {
        // Full EAS URL form (the common shape per [MS-ASHTTP]).
        assert_eq!(
            endpoint_from_x_ms_location("https://mail.contoso.com/Microsoft-Server-ActiveSync")
                .expect("valid https location must derive an endpoint"),
            "https://mail.contoso.com/Microsoft-Server-ActiveSync"
        );
        // Bare host: the fixed EAS path is appended.
        assert_eq!(
            endpoint_from_x_ms_location("https://newhost.example.com")
                .expect("valid https location must derive an endpoint"),
            "https://newhost.example.com/Microsoft-Server-ActiveSync"
        );
        // Port is preserved; a foreign path is replaced by the fixed EAS path.
        assert_eq!(
            endpoint_from_x_ms_location("https://mail.contoso.com:8443/some/other/path")
                .expect("valid https location must derive an endpoint"),
            "https://mail.contoso.com:8443/Microsoft-Server-ActiveSync"
        );
    }

    /// A location that already ends in the EAS path must not get the path
    /// doubled; the derived endpoint is always scheme + authority + the fixed
    /// path, so any path the location carries is normalized away.
    #[test]
    fn x_ms_location_eas_path_in_location_is_not_doubled() {
        assert_eq!(
            endpoint_from_x_ms_location("https://new.example.com/Microsoft-Server-ActiveSync")
                .expect("valid https location must derive an endpoint"),
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(
            endpoint_from_x_ms_location("https://new.example.com/Microsoft-Server-ActiveSync/")
                .expect("valid https location must derive an endpoint"),
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
    }

    /// Any query string the location carries is stripped — the command query
    /// string (Cmd/User/DeviceId/DeviceType) is rebuilt per request.
    #[test]
    fn x_ms_location_query_string_is_stripped() {
        assert_eq!(
            endpoint_from_x_ms_location(
                "https://new.example.com/Microsoft-Server-ActiveSync?originalReq=abc&x=1"
            )
            .expect("valid https location must derive an endpoint"),
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
    }

    /// Following an `http://` location would downgrade the connection to
    /// plaintext — rejected with a descriptive error, never silently adopted.
    #[test]
    fn x_ms_location_http_downgrade_is_rejected() {
        let err =
            endpoint_from_x_ms_location("http://mail.contoso.com/Microsoft-Server-ActiveSync")
                .expect_err("plaintext http:// location must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("http://") && msg.to_ascii_lowercase().contains("refus"),
            "error must describe the refused downgrade, got: {msg}"
        );
    }

    /// Unparseable / relative / wrong-scheme values are rejected — never
    /// silently kept on the old URL and never trusted as-is.
    #[test]
    fn x_ms_location_garbage_is_rejected() {
        for bad in [
            "",
            "not a url",
            "Microsoft-Server-ActiveSync", // relative
            "ftp://mail.contoso.com/",     // wrong scheme
            "//mail.contoso.com/Microsoft-Server-ActiveSync", // scheme-relative
        ] {
            assert!(
                endpoint_from_x_ms_location(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    /// An https URL without a host cannot become an endpoint.
    #[test]
    fn x_ms_location_hostless_url_is_rejected() {
        for bad in [
            "https://",
            "https:///?x=1",
            "https:///Microsoft-Server-ActiveSync",
        ] {
            assert!(
                endpoint_from_x_ms_location(bad).is_err(),
                "'{bad}' must be rejected (no host)"
            );
        }
    }

    /// A location carrying userinfo (`user:pass@host`, or `user@host` with no
    /// password) is REJECTED, never adopted: the credential-shaped,
    /// server-controlled string would otherwise land verbatim in the hop log
    /// and the plaintext `accounts.eas_url` column. Rejection (not silent
    /// stripping) matches the refuse-http-downgrade posture. The error must
    /// name the problem WITHOUT echoing the credential-shaped location.
    #[test]
    fn x_ms_location_userinfo_is_rejected() {
        for bad in [
            "https://user:pass@mail.example.com/Microsoft-Server-ActiveSync",
            "https://user@mail.example.com/Microsoft-Server-ActiveSync", // user, no password
            "https://user:pass@mail.example.com",                        // bare authority
            "https://user:pass@mail.example.com:8443/",                  // userinfo + port
        ] {
            let err = endpoint_from_x_ms_location(bad)
                .expect_err("location with userinfo in the authority must be rejected");
            let msg = err.to_string();
            let lower = msg.to_ascii_lowercase();
            assert!(
                lower.contains("userinfo") || lower.contains("credential"),
                "error must name the userinfo/credential problem for '{bad}', got: {msg}"
            );
            assert!(
                !msg.contains("user:pass") && !msg.contains("user@"),
                "error must not echo the credential-shaped location for '{bad}', got: {msg}"
            );
        }
    }

    /// RFC 3986: the scheme is case-insensitive. The authority's case is
    /// preserved (hostnames are case-insensitive, but we don't rewrite what
    /// the server sent).
    #[test]
    fn x_ms_location_scheme_case_insensitive_authority_preserved() {
        assert_eq!(
            endpoint_from_x_ms_location("HTTPS://Mail.Contoso.COM/owa")
                .expect("valid https location must derive an endpoint"),
            "https://Mail.Contoso.COM/Microsoft-Server-ActiveSync"
        );
    }

    /// Adopting a valid location switches the client's base URL and records
    /// the adopted target for the source layer to persist.

    #[test]
    fn adopt_redirect_location_switches_base_url_and_records_it() {
        let mut client = EasClient::new(EasConfig {
            url: "https://old.example.com/Microsoft-Server-ActiveSync".into(),
            ..EasConfig::default()
        });
        assert_eq!(client.adopted_url(), None);
        client
            .adopt_redirect_location("https://new.example.com/Microsoft-Server-ActiveSync")
            .expect("valid https location must be adopted");
        assert_eq!(
            client.config.url,
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(
            client.adopted_url(),
            Some("https://new.example.com/Microsoft-Server-ActiveSync")
        );
    }

    /// An invalid location must NOT switch the base URL and must NOT record
    /// an adoption — the error surfaces to the caller.
    #[test]
    fn adopt_redirect_location_invalid_keeps_old_url() {
        let mut client = EasClient::new(EasConfig {
            url: "https://old.example.com/Microsoft-Server-ActiveSync".into(),
            ..EasConfig::default()
        });
        assert!(
            client
                .adopt_redirect_location("http://evil.example.com/Microsoft-Server-ActiveSync")
                .is_err()
        );
        assert!(client.adopt_redirect_location("garbage").is_err());
        assert_eq!(
            client.config.url,
            "https://old.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(client.adopted_url(), None);
    }

    /// A userinfo location must NOT switch the base URL and must NOT record
    /// an adoption — the old URL stays untouched, nothing credential-shaped
    /// is persisted or recorded, and the error surfaces to the caller.
    #[test]
    fn adopt_redirect_location_userinfo_keeps_old_url() {
        let mut client = EasClient::new(EasConfig {
            url: "https://old.example.com/Microsoft-Server-ActiveSync".into(),
            ..EasConfig::default()
        });
        for bad in [
            "https://user:pass@new.example.com/Microsoft-Server-ActiveSync",
            "https://user@new.example.com/Microsoft-Server-ActiveSync", // user, no password
        ] {
            assert!(
                client.adopt_redirect_location(bad).is_err(),
                "'{bad}' must be rejected (userinfo in authority)"
            );
        }
        assert_eq!(
            client.config.url,
            "https://old.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(client.adopted_url(), None);
    }

    // The per-hop decision of the redirect follow loop ([MS-ASHTTP] §3.1.5.2)
    // is a pure boundary: follow while hops < MAX_REDIRECT_HOPS and a
    // location is present; surface the 451 at the cap or without a location.
    // The loop wiring needs a live server, but the boundary itself is
    // unit-testable.
}
