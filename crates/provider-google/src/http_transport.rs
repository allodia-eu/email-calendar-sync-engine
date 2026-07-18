//! The production reqwest [`GoogleTransport`]: bearer auth over the standard HTTP
//! stack.
//!
//! The one funnel every request flows through records the negotiated HTTP version, so
//! no path forgets to observe it. Google needs no request-shaping preference header
//! (unlike Graph's immutable-id `Prefer`): reads are plain bearer `GET`s. The write
//! verbs (a shared `POST`/`PATCH`/`DELETE` funnel) are added by the write slices as
//! they land. The offline tests drive it over a blocking single-shot mock server (no
//! network).

use async_trait::async_trait;
use engine_provider::{HttpVersion, ObservedHttpVersion};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GoogleError, transport::GoogleTransport};

/// The production reqwest transport: bearer auth.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    token: String,
    /// The HTTP version most recently observed. `GoogleClient::connect` performs no
    /// request (Google has no session-discovery step), so this stays `None` until the
    /// adapter's first fetch.
    http_version: ObservedHttpVersion,
}

impl HttpTransport {
    /// Builds a transport authenticating with an OAuth bearer access token.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`).
    pub(crate) fn new(token: String, tls: &TlsClientConfig) -> Result<Self, GoogleError> {
        Ok(Self {
            client: tls.reqwest_builder().build()?,
            token,
            http_version: ObservedHttpVersion::default(),
        })
    }

    /// Issues an authenticated write (`POST`/`PATCH`/`DELETE`) the write shapes share,
    /// recording the negotiated HTTP version — the write funnel, so every path observes
    /// it. Carries an optional `Content-Type` and `If-Match` precondition.
    async fn send_write(
        &self,
        method: reqwest::Method,
        url: &str,
        content_type: Option<&str>,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, GoogleError> {
        let mut request = self.client.request(method, url).bearer_auth(&self.token);
        if let Some(content_type) = content_type {
            request = request.header("Content-Type", content_type);
        }
        if let Some(if_match) = if_match {
            request = request.header("If-Match", if_match);
        }
        let response = request.body(body).send().await?;
        self.http_version.record(response.version());
        Ok(response)
    }
}

/// Turns a successful write response into its parsed JSON body, or `None` when the
/// action carried none (`204`). A non-2xx is a classified [`GoogleError::Status`].
async fn write_body(resp: reqwest::Response) -> Result<Option<Value>, GoogleError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(GoogleError::status(status.as_u16(), body));
    }
    let text = resp.text().await.unwrap_or_default();
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&text)?))
    }
}

#[async_trait]
impl GoogleTransport for HttpTransport {
    async fn get(&self, url: &str) -> Result<Value, GoogleError> {
        let resp = self.client.get(url).bearer_auth(&self.token).send().await?;
        self.http_version.record(resp.version());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GoogleError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
    }

    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        let resp = self
            .send_write(reqwest::Method::POST, url, Some(content_type), None, body)
            .await?;
        write_body(resp).await
    }

    async fn patch(
        &self,
        url: &str,
        content_type: &str,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        let resp = self
            .send_write(
                reqwest::Method::PATCH,
                url,
                Some(content_type),
                if_match,
                body,
            )
            .await?;
        write_body(resp).await
    }

    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GoogleError> {
        let resp = self
            .send_write(reqwest::Method::DELETE, url, None, if_match, Vec::new())
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(GoogleError::status(status.as_u16(), body))
        }
    }

    fn http_version(&self) -> Option<HttpVersion> {
        self.http_version.get()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use engine_core::error::FailureClass;

    use super::*;

    /// A blocking single-shot mock HTTP server: serves `response` to one connection, so
    /// the live reqwest transport runs offline (no network).
    fn mock_server(response: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn get_parses_a_success_body() {
        let base = mock_server(http("200 OK", r#"{"labels":[]}"#));
        let transport = HttpTransport::new("tok".to_owned(), crate::test_support::tls()).unwrap();
        let doc = transport.get(&base).await.unwrap();
        assert!(doc.get("labels").is_some());
    }

    #[tokio::test]
    async fn the_http_version_is_unknown_until_the_first_response() {
        // Google's connect performs no I/O (no session discovery), so a fresh transport
        // has observed nothing; the first response fills the fact in.
        let base = mock_server(http("200 OK", r#"{"labels":[]}"#));
        let transport = HttpTransport::new("tok".to_owned(), crate::test_support::tls()).unwrap();
        assert_eq!(GoogleTransport::http_version(&transport), None);

        transport.get(&base).await.unwrap();
        assert_eq!(
            GoogleTransport::http_version(&transport),
            Some(HttpVersion::Http1_1)
        );
    }

    #[tokio::test]
    async fn non_success_status_becomes_a_classified_status_error() {
        let body = r#"{"error":{"code":401,"message":"nope","errors":[{"reason":"authError"}],"status":"UNAUTHENTICATED"}}"#;
        let base = mock_server(http("401 Unauthorized", body));
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get(&base)
            .await
            .unwrap_err();
        assert!(matches!(&err, GoogleError::Status { reason: Some(r), .. } if r == "authError"));
        assert_eq!(err.failure_class(), FailureClass::Authentication);
    }

    #[tokio::test]
    async fn a_non_json_success_body_is_a_permanent_decode_error() {
        let base = mock_server(http("200 OK", "this is not json"));
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get(&base)
            .await
            .unwrap_err();
        assert!(matches!(err, GoogleError::Transport(_)));
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }

    #[tokio::test]
    async fn a_refused_connection_is_a_retryable_transport_error() {
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get("http://127.0.0.1:1/gmail/v1/users/me/labels")
            .await
            .unwrap_err();
        assert!(matches!(err, GoogleError::Transport(_)));
        assert!(err.failure_class().is_retryable());
    }

    #[tokio::test]
    async fn a_write_body_parses_json_or_treats_empty_as_none() {
        // A modify/send echoes JSON; a 204 (a trash/untrash no-body) carries none.
        let base = mock_server(http("200 OK", r#"{"id":"m1"}"#));
        let sent = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .post(&base, "application/json", b"{}".to_vec())
            .await
            .unwrap();
        assert_eq!(sent.unwrap()["id"], "m1");

        let base = mock_server(http("204 No Content", ""));
        HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .delete(&base, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_write_non_success_is_a_classified_status_error() {
        let body = r#"{"error":{"code":403,"message":"no","errors":[{"reason":"insufficientPermissions"}],"status":"PERMISSION_DENIED"}}"#;
        let base = mock_server(http("403 Forbidden", body));
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .post(&base, "application/json", b"{}".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }
}
