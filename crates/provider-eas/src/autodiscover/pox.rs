// SPDX-License-Identifier: MPL-2.0
// V1 POX flow: request envelope, POST + redirect handling, response tag-scan.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use super::{AutoDiscoverError, PoxOutcome};

const MAX_REDIRECTS: u8 = 3;

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
    <EMailAddress>{email}</EMailAddress>
  </Request>
</Autodiscover>"#
    )
}

/// Host-only comparison of two URLs. Unparseable input never matches (fail
/// closed — callers drop credentials on mismatch).
pub fn same_host(a: &str, b: &str) -> bool {
    let host = |u: &str| {
        reqwest::Url::parse(u)
            .ok()
            .and_then(|p| p.host_str().map(str::to_ascii_lowercase))
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
pub(super) async fn try_v1_pox(
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
        if use_auth && let Some((u, p)) = creds {
            let value = B64.encode(format!("{u}:{p}"));
            req = req.header("Authorization", format!("Basic {value}"));
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
        if (status == 301 || status == 302 || status == 303)
            && let Some(loc) = resp
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
///
/// # Errors
///
/// Returns `AutoDiscoverError::Parse` when the body is neither a Settings
/// response carrying a MobileSync URL nor a well-formed redirect.
pub fn parse_v1_pox_response(body: &str) -> Result<PoxOutcome, AutoDiscoverError> {
    if find_tag(body, "Error").is_some() {
        return Err(AutoDiscoverError::Parse("server returned <Error>".into()));
    }
    if let Some(action) = find_tag(body, "Action")
        && action.trim().eq_ignore_ascii_case("redirect")
    {
        let url = find_tag(body, "Url")
            .ok_or_else(|| AutoDiscoverError::Parse("redirect without <Url>".into()))?;
        return Ok(PoxOutcome::Redirect(url));
    }
    // MobileSync Server Url — the first <Url> in the document.
    if let Some(url) = find_tag(body, "Url") {
        return Ok(PoxOutcome::Server(url));
    }
    Err(AutoDiscoverError::NotFound)
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
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let open_start = body.find(&open)?;
    // Text starts right after the `>` that closes the opening tag. The opening
    // tag may contain attributes (`<Url foo="bar">`) so we scan forward from
    // the start of `<tag` to the first `>`.
    let text_start = body[open_start..].find('>')? + open_start + 1;
    let text_end = body[text_start..].find(&close)? + text_start;
    Some(body[text_start..text_end].trim().to_string())
}
