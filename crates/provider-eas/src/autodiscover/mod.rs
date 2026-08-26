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

mod flow;
mod pox;
mod srv;
#[cfg(test)]
mod tests;
mod v2;

pub use flow::autodiscover;
pub use pox::{build_v1_pox_body, parse_v1_pox_response, same_host};
pub use srv::{SrvRecordData, srv_autodiscover_url, srv_query_name};
pub use v2::{build_v2_url, parse_v2_json_response};

/// Errors of the autodiscover flow.
#[derive(Debug, thiserror::Error)]
pub enum AutoDiscoverError {
    /// A candidate URL answered a non-2xx HTTP status.
    #[error("HTTP {status}: {body}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// (Possibly truncated) response body for diagnostics.
        body: String,
    },
    /// The HTTP request itself failed (DNS, TLS, timeout).
    #[error("transport: {0}")]
    Transport(String),
    /// The response body could not be parsed as POX/JSON.
    #[error("parse: {0}")]
    Parse(String),
    /// The redirect chain exceeded `MAX_REDIRECTS` hops.
    #[error("redirect loop exceeded {0} hops")]
    TooManyRedirects(u8),
    /// No flow (V1 POX, V2 JSON, DNS SRV) yielded an EAS URL.
    #[error("no EAS URL found in any flow")]
    NotFound,
}

/// Successful autodiscover outcome: the resolved EAS endpoint.
#[derive(Debug, Clone)]
pub struct AutodiscoverResult {
    /// The `Microsoft-Server-ActiveSync` URL to configure the client with.
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
