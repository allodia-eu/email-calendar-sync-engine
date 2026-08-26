// SPDX-License-Identifier: MPL-2.0
// V2 JSON flow: the autodiscover.json endpoint and its response shape.

use serde::Deserialize;

use super::AutoDiscoverError;

/// The V2 JSON endpoint URL. `Protocol=ActiveSync` is required — without it
/// the endpoint answers 400 Protocol_MissingProtocol.
pub fn build_v2_url(email: &str) -> String {
    format!(
        "https://autodiscover-s.outlook.com/autodiscover/autodiscover.json?Email={email}&Protocol=ActiveSync"
    )
}

/// GET the V2 JSON endpoint and read `Url`.
pub(super) async fn try_v2_json(
    email: &str,
    http: &reqwest::Client,
) -> Result<String, AutoDiscoverError> {
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
///
/// # Errors
///
/// Returns `AutoDiscoverError::Parse` when the body is not the V2 JSON shape or
/// carries an Error element.
pub fn parse_v2_json_response(body: &str) -> Result<String, AutoDiscoverError> {
    let parsed: V2Response = serde_json::from_str(body)
        .map_err(|e| AutoDiscoverError::Parse(format!("V2 JSON: {e}")))?;
    Ok(parsed.url)
}
