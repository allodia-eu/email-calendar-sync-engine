// SPDX-License-Identifier: MPL-2.0
// The full autodiscover flow: V1 POX, then V2 JSON, then DNS SRV.

use super::{
    AutoDiscoverError, AutodiscoverResult,
    pox::try_v1_pox,
    srv::{srv_autodiscover_url, srv_lookup_records, srv_query_name},
    v2::try_v2_json,
};

/// Run the full flow: V1 POX (with redirects) on the email's domain and
/// `autodiscover.<domain>`, then V2 JSON fallback for Exchange Online, then
/// the DNS SRV fallback ([MS-ASCMD] §4.2 step 7) as the LAST resort.
///
/// `http` must be the host's shared client — built from its
/// `TlsClientConfig::reqwest_builder` (`docs/agent-guidance/tls.md`) — so
/// autodiscover resolves under the same TLS trust policy as the EAS commands
/// that follow against the discovered URL.
///
/// V1 is tried first because on-prem Exchange won't be reachable via the V2
/// Outlook Online endpoint; V2 is the reliable fallback for M365 mailboxes.
///
/// `creds` (username, password) is used for V1 Basic auth ONLY after the
/// anonymous request is answered 401 — see the module docs.
///
/// Failure reporting: every attempted path is recorded; the surfaced error is
/// the ORIGINAL candidate-URL error (not a synthetic "SRV failed"), with the
/// full attempt list (including the SRV attempt) warn-logged — no path is
/// silently swallowed.
///
/// # Errors
///
/// Returns `AutoDiscoverError` — the ORIGINAL candidate-URL error once every
/// flow (V1 POX, V2 JSON, DNS SRV) has failed, never a synthetic "not found";
/// all attempts are warn-logged.
pub async fn autodiscover(
    email: &str,
    http: &reqwest::Client,
    creds: Option<(&str, &str)>,
) -> Result<AutodiscoverResult, AutoDiscoverError> {
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d)
        .ok_or_else(|| AutoDiscoverError::Parse(format!("not an email: {email}")))?;
    let v1_candidates = [
        format!("https://{domain}/autodiscover/autodiscover.xml"),
        format!("https://autodiscover.{domain}/autodiscover/autodiscover.xml"),
    ];
    let mut attempts: Vec<String> = Vec::new();
    let mut original_error: Option<AutoDiscoverError> = None;
    for base in v1_candidates {
        match try_v1_pox(base.clone(), email, http, creds).await {
            Ok(url) => {
                log::info!("AutoDiscover resolved via candidate URL {base}");
                return Ok(AutodiscoverResult { eas_url: url });
            }
            Err(e) => {
                log::debug!("AutoDiscover V1 {base} failed: {e}");
                attempts.push(format!("V1 {base}: {e}"));
                if original_error.is_none() {
                    original_error = Some(e);
                }
            }
        }
    }
    // V2 fallback.
    match try_v2_json(email, http).await {
        Ok(url) => {
            log::info!("AutoDiscover resolved via V2 JSON endpoint");
            return Ok(AutodiscoverResult { eas_url: url });
        }
        Err(e) => {
            log::debug!("AutoDiscover V2 failed: {e}");
            attempts.push(format!("V2: {e}"));
            if original_error.is_none() {
                original_error = Some(e);
            }
        }
    }
    // DNS SRV fallback — [MS-ASCMD] §4.2 step 7, LAST resort.
    match srv_lookup_records(domain).await {
        Ok(records) => {
            if let Some(url) = srv_autodiscover_url(&records) {
                match try_v1_pox(url.clone(), email, http, creds).await {
                    Ok(eas_url) => {
                        log::info!("AutoDiscover resolved via DNS SRV target ({url})");
                        return Ok(AutodiscoverResult { eas_url });
                    }
                    Err(e) => {
                        log::warn!("AutoDiscover SRV-derived URL {url} failed: {e}");
                        attempts.push(format!("SRV {url}: {e}"));
                    }
                }
            } else {
                log::info!(
                    "AutoDiscover SRV {} returned no usable record",
                    srv_query_name(domain)
                );
                attempts.push(format!("SRV {}: no usable record", srv_query_name(domain)));
            }
        }
        Err(e) => {
            log::warn!(
                "AutoDiscover SRV lookup {} failed: {}",
                srv_query_name(domain),
                e
            );
            attempts.push(format!("SRV {}: {}", srv_query_name(domain), e));
        }
    }
    log::warn!(
        "AutoDiscover failed on all paths for {}: [{}]",
        email,
        attempts.join(" | ")
    );
    Err(original_error.unwrap_or(AutoDiscoverError::NotFound))
}
