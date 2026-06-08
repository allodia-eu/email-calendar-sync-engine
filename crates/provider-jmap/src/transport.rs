//! reqwest-backed HTTP transport for JMAP.
//!
//! Thin wrapper that applies authentication, ships the JSON envelope, and maps
//! HTTP/transport failures into [`JmapError`]. Redirects are **not** auto-followed:
//! the session-discovery flow in [`crate`] resolves the well-known redirect itself
//! so it can rebase a foreign advertised origin onto the connection (see
//! [`SessionUrlPolicy`](crate::SessionUrlPolicy)).

use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder};
use serde_json::Value;

use crate::Credentials;
use crate::error::JmapError;

/// An authenticated HTTP transport.
pub(crate) struct Transport {
    client: Client,
    credentials: Credentials,
}

impl Transport {
    /// Builds a transport with redirect-following disabled.
    pub(crate) fn new(credentials: Credentials) -> Result<Self, JmapError> {
        let client = Client::builder().redirect(Policy::none()).build()?;
        Ok(Self {
            client,
            credentials,
        })
    }

    /// Applies the configured credentials to a request builder.
    fn authed(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
        }
    }

    /// Sends an authenticated GET, returning the raw response so the caller can
    /// inspect a redirect's status and `Location` before reading any body.
    pub(crate) async fn get(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        Ok(self.authed(self.client.get(url)).send().await?)
    }

    /// POSTs `body` as JSON and parses a success response as a JSON value.
    pub(crate) async fn post_json(&self, url: &str, body: &Value) -> Result<Value, JmapError> {
        let resp = self.authed(self.client.post(url)).json(body).send().await?;
        read_json(resp).await
    }
}

/// Reads a JSON body, mapping a non-success status to [`JmapError::Status`] with
/// the body captured for diagnostics.
pub(crate) async fn read_json(resp: reqwest::Response) -> Result<Value, JmapError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(JmapError::status(status.as_u16(), body));
    }
    Ok(resp.json::<Value>().await?)
}
