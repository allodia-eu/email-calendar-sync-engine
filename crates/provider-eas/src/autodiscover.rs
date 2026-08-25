// SPDX-License-Identifier: MPL-2.0
//! Exchange AutoDiscover — resolves the EAS URL for a user's email.
//!
//! Two flows, tried in order, plus a final DNS fallback:
//!   1. V1 POX — POST the mobilesync XML envelope (document xmlns is the
//!      mobilesync REQUEST schema per [MS-OXDISCO]; the outlook request
//!      schema makes Exchange answer 600 Invalid Request) to
//!      `https://<domain>/autodiscover/autodiscover.xml` (and the same path on
//!      `autodiscover.<domain>`). Parse the `<MobileSync><Server><Url>` from
//!      the XML response. Follow `<Redirect><Url>` up to MAX_REDIRECTS hops.
//!   2. V2 JSON — GET `https://autodiscover-s.outlook.com/autodiscover/autodiscover.json?Email=<email>&Protocol=ActiveSync`
//!      and read `Url` from the JSON. The V2 endpoint returns the canonical Exchange Online EAS URL
//!      for any M365 mailbox.
//!   3. DNS SRV — [MS-ASCMD] §4.2 step 7: query `_autodiscover._tcp.<domain>`
//!      and POST the V1 POX flow to `https://<target>/autodiscover/autodiscover.xml`
//!      (SRV port when not 443). This is the LAST fallback, after both URL
//!      flows fail; on-prem Exchange deployments commonly publish only the
//!      SRV record.
//!
//! HTTP 301/302/303 redirects on the V1 endpoint also count toward
//! MAX_REDIRECTS. POX `<Action>redirect</Action>` + `<Redirect><Url>` is the
//! in-body redirect signal.
//!
//! Auth: V1 is sent ANONYMOUSLY first — many servers answer autodiscover
//! without credentials, and we don't leak them to servers that don't ask.
//! Only after a 401 (and only when the caller supplied credentials) is the
//! request retried with HTTP Basic. The Authorization header is NEVER
//! forwarded across a cross-host redirect: a host change drops back to
//! anonymous (which may re-trigger the 401 → auth upgrade on the new host).
//!
//! Parsing note: the POX response is parsed with a regex-free tag-scan (the
//! `find_tag` helper), NOT a full XML parser. The response shape is server-
//! controlled and stable, and the EAS crate already ships a hand-written
//! WBXML codec; adding `quick-xml` for ~3 tags is out of proportion. Robust
//! XML parsing is a documented deferred hardening item.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;

const MAX_REDIRECTS: u8 = 3;

#[derive(Debug, thiserror::Error)]
pub enum AutoDiscoverError {
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("redirect loop exceeded {0} hops")]
    TooManyRedirects(u8),
    #[error("no EAS URL found in any flow")]
    NotFound,
}

#[derive(Debug, Clone)]
pub struct AutodiscoverResult {
    pub eas_url: String,
}

/// Outcome of parsing one V1 POX response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoxOutcome {
    /// `<MobileSync><Server><Url>` — the EAS endpoint to use.
    Server(String),
    /// `<Action>redirect</Action>` + `<Redirect><Url>` — re-issue the request
    /// to this URL (counts toward MAX_REDIRECTS).
    Redirect(String),
}

/// Run the full flow: V1 POX (with redirects) on the email's domain and
/// `autodiscover.<domain>`, then V2 JSON fallback for Exchange Online, then
/// the DNS SRV fallback ([MS-ASCMD] §4.2 step 7) as the LAST resort.
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
pub async fn autodiscover(
    email: &str,
    http: &reqwest::Client,
    creds: Option<(&str, &str)>,
) -> Result<AutodiscoverResult, AutoDiscoverError> {
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d)
        .ok_or_else(|| AutoDiscoverError::Parse(format!("not an email: {}", email)))?;
    let v1_candidates = [
        format!("https://{}/autodiscover/autodiscover.xml", domain),
        format!(
            "https://autodiscover.{}/autodiscover/autodiscover.xml",
            domain
        ),
    ];
    let mut attempts: Vec<String> = Vec::new();
    let mut original_error: Option<AutoDiscoverError> = None;
    for base in v1_candidates {
        match try_v1_pox(base.clone(), email, http, creds).await {
            Ok(url) => {
                log::info!("AutoDiscover resolved via candidate URL {}", base);
                return Ok(AutodiscoverResult { eas_url: url });
            }
            Err(e) => {
                log::debug!("AutoDiscover V1 {} failed: {}", base, e);
                attempts.push(format!("V1 {}: {}", base, e));
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
            log::debug!("AutoDiscover V2 failed: {}", e);
            attempts.push(format!("V2: {}", e));
            if original_error.is_none() {
                original_error = Some(e);
            }
        }
    }
    // DNS SRV fallback — [MS-ASCMD] §4.2 step 7, LAST resort.
    match srv_lookup_records(domain).await {
        Ok(records) => match srv_autodiscover_url(&records) {
            Some(url) => match try_v1_pox(url.clone(), email, http, creds).await {
                Ok(eas_url) => {
                    log::info!("AutoDiscover resolved via DNS SRV target ({})", url);
                    return Ok(AutodiscoverResult { eas_url });
                }
                Err(e) => {
                    log::warn!("AutoDiscover SRV-derived URL {} failed: {}", url, e);
                    attempts.push(format!("SRV {}: {}", url, e));
                }
            },
            None => {
                log::info!(
                    "AutoDiscover SRV {} returned no usable record",
                    srv_query_name(domain)
                );
                attempts.push(format!("SRV {}: no usable record", srv_query_name(domain)));
            }
        },
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

/// Plain-data SRV record shape, decoupled from hickory so record selection is
/// unit-testable without a DNS resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvRecordData {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    /// Target host, possibly with a trailing root dot (as DNS names arrive).
    pub target: String,
}

/// The SRV query name for [MS-ASCMD] §4.2 step 7. Fully qualified (trailing
/// dot) so the resolver treats it as absolute and skips search-suffix
/// expansion.
pub fn srv_query_name(domain: &str) -> String {
    format!("_autodiscover._tcp.{}.", domain.trim_end_matches('.'))
}

/// RFC 2782 record selection, made deterministic for reproducibility: lowest
/// priority wins; among equal priority the highest weight wins; full ties
/// resolve to the FIRST record in the list. (RFC 2782 §"Weight" specifies a
/// randomized load-balance for equal priorities; a mail client discovers once
/// per account setup, so a deterministic pick is preferable to RNG here.)
///
/// Builds the autodiscover URL for the winning record: port 443 is omitted,
/// any other port is explicit, and the trailing root dot on the target is
/// stripped. A root (".") target means "service decidedly not available at
/// this domain" (RFC 2782) and yields `None`, as does an empty record list.
pub fn srv_autodiscover_url(records: &[SrvRecordData]) -> Option<String> {
    let best = records
        .iter()
        .enumerate()
        .min_by(|(ia, a), (ib, b)| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| b.weight.cmp(&a.weight))
                .then_with(|| ia.cmp(ib))
        })
        .map(|(_, r)| r)?;
    let host = best.target.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    match best.port {
        443 => Some(format!("https://{}/autodiscover/autodiscover.xml", host)),
        port => Some(format!(
            "https://{}:{}/autodiscover/autodiscover.xml",
            host, port
        )),
    }
}

/// Thin async wrapper around hickory-resolver: system DNS config
/// (/etc/resolv.conf / Windows registry), SRV query for
/// `_autodiscover._tcp.<domain>`. Deliberately thin — the DNS exchange itself
/// cannot be unit-tested without a live resolver; ALL selection logic lives
/// in `srv_autodiscover_url` (unit-tested).
async fn srv_lookup_records(domain: &str) -> Result<Vec<SrvRecordData>, AutoDiscoverError> {
    let resolver = hickory_resolver::Resolver::builder_tokio()
        .map_err(|e| AutoDiscoverError::Transport(format!("DNS resolver init: {}", e)))?
        .build()
        .map_err(|e| AutoDiscoverError::Transport(format!("DNS resolver build: {}", e)))?;
    let name = srv_query_name(domain);
    let lookup = resolver
        .srv_lookup(&name)
        .await
        .map_err(|e| AutoDiscoverError::Transport(format!("SRV {}: {}", name, e)))?;
    let records = lookup
        .answers()
        .iter()
        .filter_map(|record| match &record.data {
            hickory_resolver::proto::rr::RData::SRV(srv) => Some(SrvRecordData {
                priority: srv.priority,
                weight: srv.weight,
                port: srv.port,
                target: srv.target.to_string(),
            }),
            _ => None,
        })
        .collect();
    Ok(records)
}

/// The V1 POX request envelope. Per [MS-OXDISCO] the document xmlns of a
/// mobilesync autodiscover request is the MOBILESYNC request schema
/// (Android's EasAutodiscover does the same); the AcceptableResponseSchema
/// declares the mobilesync RESPONSE schema.
pub fn build_v1_pox_body(email: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006">
  <Request>
    <AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006</AcceptableResponseSchema>
    <EMailAddress>{}</EMailAddress>
  </Request>
</Autodiscover>"#,
        email
    )
}

/// The V2 JSON endpoint URL. `Protocol=ActiveSync` is required — without it
/// the endpoint answers 400 Protocol_MissingProtocol.
pub fn build_v2_url(email: &str) -> String {
    format!(
        "https://autodiscover-s.outlook.com/autodiscover/autodiscover.json?Email={}&Protocol=ActiveSync",
        email
    )
}

/// Host-only comparison of two URLs. Unparseable input never matches (fail
/// closed — callers drop credentials on mismatch).
pub fn same_host(a: &str, b: &str) -> bool {
    let host = |u: &str| {
        reqwest::Url::parse(u)
            .ok()
            .and_then(|p| p.host_str().map(|h| h.to_ascii_lowercase()))
    };
    matches!((host(a), host(b)), (Some(x), Some(y)) if x == y)
}

/// POST the mobilesync envelope to `url` and follow HTTP + POX redirects up to
/// MAX_REDIRECTS hops. Returns the resolved EAS URL on `PoxOutcome::Server`.
///
/// Anonymous-first auth: the first request carries no Authorization header.
/// A 401 with caller-supplied creds flips `use_auth` and retries WITH Basic.
/// A cross-host redirect (HTTP 30x or POX `<Redirect>`) resets `use_auth` —
/// credentials are never forwarded to a different host.
async fn try_v1_pox(
    url: String,
    email: &str,
    http: &reqwest::Client,
    creds: Option<(&str, &str)>,
) -> Result<String, AutoDiscoverError> {
    let body = build_v1_pox_body(email);
    let mut current_url = url;
    let mut use_auth = false;
    for _ in 0..MAX_REDIRECTS {
        let mut req = http
            .post(&current_url)
            .header("Content-Type", "text/xml")
            .body(body.clone());
        if use_auth {
            if let Some((u, p)) = creds {
                let value = B64.encode(format!("{}:{}", u, p));
                req = req.header("Authorization", format!("Basic {}", value));
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AutoDiscoverError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 401 && !use_auth && creds.is_some() {
            // Server demands auth — retry this same URL with Basic.
            use_auth = true;
            continue;
        }
        if status == 301 || status == 302 || status == 303 {
            if let Some(loc) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            {
                if !same_host(&current_url, loc) {
                    use_auth = false;
                }
                current_url = loc.to_string();
                continue;
            }
        }
        if status != 200 {
            let b = resp.text().await.unwrap_or_default();
            return Err(AutoDiscoverError::HttpStatus { status, body: b });
        }
        let text = resp
            .text()
            .await
            .map_err(|e| AutoDiscoverError::Transport(e.to_string()))?;
        match parse_v1_pox_response(&text)? {
            PoxOutcome::Server(u) => return Ok(u),
            PoxOutcome::Redirect(u) => {
                if !same_host(&current_url, &u) {
                    use_auth = false;
                }
                current_url = u;
                continue;
            }
        }
    }
    Err(AutoDiscoverError::TooManyRedirects(MAX_REDIRECTS))
}

/// Parse a V1 POX response body. Tag-scan (NOT a full XML parser) — see the
/// module docs.
///
/// - `<Error>` anywhere → `Parse` error.
/// - `<Action>redirect</Action>` (case-insensitive whitespace trim) → `Redirect(<Redirect><Url>)`.
/// - Otherwise the first `<Url>` (the `<MobileSync><Server><Url>`) → `Server`.
/// - None of the above → `NotFound`.
pub fn parse_v1_pox_response(body: &str) -> Result<PoxOutcome, AutoDiscoverError> {
    if find_tag(body, "Error").is_some() {
        return Err(AutoDiscoverError::Parse("server returned <Error>".into()));
    }
    if let Some(action) = find_tag(body, "Action") {
        if action.trim().eq_ignore_ascii_case("redirect") {
            let url = find_tag(body, "Url")
                .ok_or_else(|| AutoDiscoverError::Parse("redirect without <Url>".into()))?;
            return Ok(PoxOutcome::Redirect(url));
        }
    }
    // MobileSync Server Url — the first <Url> in the document.
    if let Some(url) = find_tag(body, "Url") {
        return Ok(PoxOutcome::Server(url));
    }
    Err(AutoDiscoverError::NotFound)
}

/// GET the V2 JSON endpoint and read `Url`.
async fn try_v2_json(email: &str, http: &reqwest::Client) -> Result<String, AutoDiscoverError> {
    let url = build_v2_url(email);
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| AutoDiscoverError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let b = resp.text().await.unwrap_or_default();
        return Err(AutoDiscoverError::HttpStatus { status, body: b });
    }
    let text = resp
        .text()
        .await
        .map_err(|e| AutoDiscoverError::Transport(e.to_string()))?;
    parse_v2_json_response(&text)
}

#[derive(Deserialize)]
struct V2Response {
    #[serde(rename = "Url")]
    url: String,
    #[serde(rename = "Protocol", default = "default_protocol")]
    _protocol: String,
}
fn default_protocol() -> String {
    String::new()
}

/// Parse the V2 JSON response: `{"Url":"...","Protocol":"ActiveSync"}`. Only
/// `Url` is required; `Protocol` is ignored (defaulted if absent).
pub fn parse_v2_json_response(body: &str) -> Result<String, AutoDiscoverError> {
    let parsed: V2Response = serde_json::from_str(body)
        .map_err(|e| AutoDiscoverError::Parse(format!("V2 JSON: {}", e)))?;
    Ok(parsed.url)
}

/// Find the inner text of the first `<tag ...>...</tag>` occurrence. Naive
/// tag-scan — does NOT handle namespaces, CDATA, or self-closing tags, and
/// only finds the FIRST occurrence. Sufficient for AutoDiscover's fixed
/// server-controlled response shape.
///
/// Handles an opening tag with attributes (e.g. `<Url foo="bar">`) by scanning
/// forward from `<tag` to the `>` that closes the opening tag; the inner text
/// starts immediately after that `>`.
fn find_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let open_start = body.find(&open)?;
    // Text starts right after the `>` that closes the opening tag. The opening
    // tag may contain attributes (`<Url foo="bar">`) so we scan forward from
    // the start of `<tag` to the first `>`.
    let text_start = body[open_start..].find('>')? + open_start + 1;
    let text_end = body[text_start..].find(&close)? + text_start;
    Some(body[text_start..text_end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        PoxOutcome, SrvRecordData, build_v1_pox_body, build_v2_url, parse_v1_pox_response,
        parse_v2_json_response, same_host, srv_autodiscover_url, srv_query_name,
    };

    #[test]
    fn v1_pox_body_uses_mobilesync_request_schema() {
        // [MS-OXDISCO]: a mobilesync autodiscover request's document xmlns
        // must be the MOBILESYNC request schema — the outlook request schema
        // makes Exchange answer 600 Invalid Request (live-probed).
        let body = build_v1_pox_body("alice@example.com");
        assert!(body.contains(
            r#"xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006""#
        ));
        assert!(body.contains(
            "http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006"
        ));
        assert!(body.contains("<EMailAddress>alice@example.com</EMailAddress>"));
        assert!(!body.contains("outlook/requestschema"));
    }

    #[test]
    fn v2_url_declares_activesync_protocol() {
        // Without &Protocol=ActiveSync the V2 endpoint 400s
        // (Protocol_MissingProtocol — live-probed).
        let url = build_v2_url("alice@example.com");
        assert!(url.contains("Email=alice@example.com"));
        assert!(url.contains("Protocol=ActiveSync"));
    }

    #[test]
    fn same_host_compares_url_hosts() {
        assert!(same_host(
            "https://mail.contoso.com/autodiscover/autodiscover.xml",
            "https://mail.contoso.com/other/path"
        ));
        assert!(!same_host(
            "https://mail.contoso.com/autodiscover/autodiscover.xml",
            "https://contoso.onmicrosoft.com/autodiscover/autodiscover.xml"
        ));
        // Unparseable URLs never match — fail closed (drop auth).
        assert!(!same_host("not a url", "https://mail.contoso.com/"));
    }

    #[test]
    fn parse_v1_pox_extracts_server_url() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
  <Response>
    <User><AutoDiscoverEmail>alice@example.com</AutoDiscoverEmail></User>
    <Action>Settings</Action>
    <MobileSync>
      <Server>
        <Type>MobileSync</Type>
        <Url>https://mail.contoso.com/Microsoft-Server-ActiveSync</Url>
        <Name>https://mail.contoso.com/Microsoft-Server-ActiveSync</Name>
      </Server>
    </MobileSync>
  </Response>
</Autodiscover>"#;
        let parsed = parse_v1_pox_response(body).unwrap();
        match parsed {
            PoxOutcome::Server(url) => {
                assert_eq!(url, "https://mail.contoso.com/Microsoft-Server-ActiveSync")
            }
            _ => panic!("expected Server outcome"),
        }
    }

    #[test]
    fn parse_v1_pox_returns_redirect_when_action_redirect() {
        let body = r#"<Autodiscover xmlns="...">
      <Response><Action>redirect</Action><Redirect><Url>https://contoso.onmicrosoft.com/autodiscover/autodiscover.xml</Url></Redirect></Response>
    </Autodiscover>"#;
        let parsed = parse_v1_pox_response(body).unwrap();
        match parsed {
            PoxOutcome::Redirect(url) => assert!(url.contains("contoso.onmicrosoft.com")),
            _ => panic!("expected Redirect"),
        }
    }

    #[test]
    fn parse_v2_json_extracts_url() {
        let body = r#"{"Url":"https://outlook.office365.com/Microsoft-Server-ActiveSync","Protocol":"ActiveSync"}"#;
        let url = parse_v2_json_response(body).unwrap();
        assert_eq!(
            url,
            "https://outlook.office365.com/Microsoft-Server-ActiveSync"
        );
    }

    #[test]
    fn parse_v1_pox_rejects_error_response() {
        let body = r#"<Autodiscover xmlns="..."><Response><Error><ErrorCode>500</ErrorCode><Message>Invalid request</Message></Error></Response></Autodiscover>"#;
        assert!(parse_v1_pox_response(body).is_err());
    }

    // --- DNS SRV autodiscover fallback ([MS-ASCMD] §4.2 step 7) -------------
    //
    // `srv_autodiscover_url` record-selection truth table. The DNS query
    // itself is NOT unit-testable (it needs a live resolver); the selection +
    // URL-construction logic is pure and covered here.

    fn srv(priority: u16, weight: u16, port: u16, target: &str) -> SrvRecordData {
        SrvRecordData {
            priority,
            weight,
            port,
            target: target.to_string(),
        }
    }

    #[test]
    fn srv_empty_record_list_yields_none() {
        assert_eq!(srv_autodiscover_url(&[]), None);
    }

    #[test]
    fn srv_lowest_priority_wins() {
        let records = [
            srv(10, 100, 443, "mail-a.contoso.com."),
            srv(5, 0, 443, "mail-b.contoso.com."),
        ];
        assert_eq!(
            srv_autodiscover_url(&records),
            Some("https://mail-b.contoso.com/autodiscover/autodiscover.xml".to_string())
        );
    }

    #[test]
    fn srv_same_priority_highest_weight_wins() {
        let records = [
            srv(10, 10, 443, "mail-a.contoso.com."),
            srv(10, 50, 443, "mail-b.contoso.com."),
        ];
        assert_eq!(
            srv_autodiscover_url(&records),
            Some("https://mail-b.contoso.com/autodiscover/autodiscover.xml".to_string())
        );
    }

    #[test]
    fn srv_full_tie_picks_first() {
        // Deterministic tie-break for tests: identical priority AND weight →
        // the first record in the list wins (real RFC 2782 weighting is
        // randomized; we pick determinism for reproducibility).
        let records = [
            srv(10, 10, 443, "mail-a.contoso.com."),
            srv(10, 10, 443, "mail-b.contoso.com."),
        ];
        assert_eq!(
            srv_autodiscover_url(&records),
            Some("https://mail-a.contoso.com/autodiscover/autodiscover.xml".to_string())
        );
    }

    #[test]
    fn srv_port_443_omitted_from_url() {
        let records = [srv(0, 0, 443, "mail.contoso.com.")];
        assert_eq!(
            srv_autodiscover_url(&records),
            Some("https://mail.contoso.com/autodiscover/autodiscover.xml".to_string())
        );
    }

    #[test]
    fn srv_custom_port_included_in_url() {
        let records = [srv(0, 0, 8443, "mail.contoso.com.")];
        assert_eq!(
            srv_autodiscover_url(&records),
            Some("https://mail.contoso.com:8443/autodiscover/autodiscover.xml".to_string())
        );
    }

    #[test]
    fn srv_trailing_dot_on_target_stripped() {
        let records = [srv(0, 0, 443, "mail.contoso.com.")];
        let url = srv_autodiscover_url(&records).unwrap();
        assert!(!url.contains("com.."), "double dot leaked: {}", url);
        assert!(url.starts_with("https://mail.contoso.com/"));
    }

    #[test]
    fn srv_root_target_means_service_unavailable() {
        // RFC 2782: a target of "." means "decidedly not available".
        let records = [srv(0, 0, 443, ".")];
        assert_eq!(srv_autodiscover_url(&records), None);
    }

    #[test]
    fn srv_query_name_is_autodiscover_tcp_domain() {
        assert_eq!(
            srv_query_name("contoso.com"),
            "_autodiscover._tcp.contoso.com."
        );
    }
}
