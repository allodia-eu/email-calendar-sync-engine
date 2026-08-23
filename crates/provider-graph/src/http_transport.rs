//! The production reqwest [`GraphTransport`]: bearer auth + immutable-id preference.
//!
//! The one funnel every request flows through records the negotiated HTTP version, so
//! no path forgets to observe it. Reads add `Prefer: IdType="ImmutableId"` (and a
//! calendar read's `outlook.timezone`); writes add an optional `Content-Type`/`If-Match`.
//! The offline tests drive it over a blocking single-shot mock server (no network).

use async_trait::async_trait;
use engine_http::{RetryConfig, send_retrying};
use engine_provider::{HttpVersion, ObservedHttpVersion};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GraphError, transport::GraphTransport};

/// The production reqwest transport: bearer auth + immutable-id preference.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    token: String,
    /// The HTTP version most recently observed. Unlike JMAP/CalDAV,
    /// `GraphClient::connect` performs no request (Graph has no session-discovery step),
    /// so this stays `None` until the adapter's first fetch.
    http_version: ObservedHttpVersion,
    /// How a `429` is waited out. Exchange Online throttles a mailbox on both concurrency
    /// and a requests-per-window budget, and names its own wait in `Retry-After`.
    retry: RetryConfig,
}

impl HttpTransport {
    /// Builds a transport authenticating with an OAuth bearer access token.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`) and `retry` its
    /// throttling policy (`docs/agent-guidance/http-throttling.md`).
    pub(crate) fn new(
        token: String,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
    ) -> Result<Self, GraphError> {
        Ok(Self {
            client: tls.reqwest_builder().build()?,
            token,
            http_version: ObservedHttpVersion::default(),
            retry: retry.clone().labelled("graph"),
        })
    }

    /// Issues the authenticated, immutable-id-preferring `GET` the fetch shapes share,
    /// recording the negotiated HTTP version on the way through — the one funnel, so no
    /// path can forget to observe it. `extra_prefer` appends a second `Prefer` value
    /// (the calendar read's `outlook.timezone`).
    async fn send(
        &self,
        url: &str,
        extra_prefer: Option<&str>,
    ) -> Result<reqwest::Response, GraphError> {
        let prefer = match extra_prefer {
            Some(extra) => format!("IdType=\"ImmutableId\", {extra}"),
            None => "IdType=\"ImmutableId\"".to_owned(),
        };
        let response = send_retrying(
            self.client
                .get(url)
                .bearer_auth(&self.token)
                .header("Prefer", prefer),
            &self.retry,
        )
        .await?;
        self.http_version.record(response.version());
        Ok(response)
    }

    /// Issues an authenticated write (`POST`/`PATCH`/`DELETE`) the write shapes share,
    /// recording the negotiated HTTP version like [`send`](Self::send) — the write
    /// counterpart, so both funnels observe the version. Carries the immutable-id
    /// preference so a write that echoes an object (a create/patch) returns the stable
    /// id form, plus an optional `Content-Type` and `If-Match` precondition.
    async fn send_write(
        &self,
        method: reqwest::Method,
        url: &str,
        content_type: Option<&str>,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, GraphError> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Prefer", "IdType=\"ImmutableId\"");
        if let Some(content_type) = content_type {
            request = request.header("Content-Type", content_type);
        }
        if let Some(if_match) = if_match {
            request = request.header("If-Match", if_match);
        }
        // A bodyless `POST` action (Graph `permanentDelete`) needs an explicit
        // `Content-Length: 0`: reqwest omits the header for an empty body, and Graph
        // answers such a `POST` with `411 Length Required`. (`DELETE` needs no length,
        // so the extra header is harmless there.)
        if body.is_empty() {
            request = request.header(reqwest::header::CONTENT_LENGTH, 0);
        }
        let response = send_retrying(request.body(body), &self.retry).await?;
        self.http_version.record(response.version());
        Ok(response)
    }

    /// Turns a byte response into its body, classifying a non-2xx. Shared by the
    /// authenticated and anonymous byte paths so they differ in exactly one thing:
    /// whether the bearer token is attached.
    async fn collect_bytes(resp: reqwest::Response) -> Result<Vec<u8>, GraphError> {
        let status = resp.status();
        if !status.is_success() {
            // The `$value` error body is JSON like any other Graph error, so classify it
            // the same way (an expired/moved message → the caller re-syncs and retries).
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

/// Turns a successful write response into its parsed JSON body, or `None` when the
/// action carried none (`202`/`204`). A non-2xx is a classified [`GraphError::Status`].
async fn write_body(resp: reqwest::Response) -> Result<Option<Value>, GraphError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(GraphError::status(status.as_u16(), body));
    }
    let text = resp.text().await.unwrap_or_default();
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&text)?))
    }
}

#[async_trait]
impl GraphTransport for HttpTransport {
    async fn get(&self, url: &str) -> Result<Value, GraphError> {
        let resp = self.send(url, None).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
    }

    async fn get_with_prefer(&self, url: &str, prefer: Option<&str>) -> Result<Value, GraphError> {
        let resp = self.send(url, prefer).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        let resp = self.send(url, None).await?;
        Self::collect_bytes(resp).await
    }

    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        // No `Authorization` and no `Prefer`: this URL came from payload content, not
        // from the Graph API, so it gets a bare GET.
        let resp = send_retrying(self.client.get(url), &self.retry).await?;
        self.http_version.record(resp.version());
        Self::collect_bytes(resp).await
    }

    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
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
    ) -> Result<Option<Value>, GraphError> {
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

    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GraphError> {
        let resp = self
            .send_write(reqwest::Method::DELETE, url, None, if_match, Vec::new())
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(GraphError::status(status.as_u16(), body))
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

    /// Serves one canned response and hands back the raw request head, so a test can
    /// assert on the headers that actually went out.
    fn mock_server_capturing(
        response: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).unwrap_or(0);
                *sink.lock().unwrap() = String::from_utf8_lossy(&buf[..read]).into_owned();
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// A contact payload can carry a photo URI naming any host. The anonymous byte
    /// path exists so the account's OAuth token never reaches such a host.
    #[tokio::test]
    async fn the_anonymous_byte_path_sends_no_authorization_header() {
        let (base, seen) = mock_server_capturing(http("200 OK", "bytes"));
        let transport = HttpTransport::new(
            "super-secret".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap();

        transport.get_bytes_unauthenticated(&base).await.unwrap();

        let sent = seen.lock().unwrap().to_lowercase();
        assert!(
            !sent.contains("authorization"),
            "OAuth token leaked to a payload-named host: {sent}"
        );
        assert!(
            !sent.contains("super-secret"),
            "token in the request: {sent}"
        );
    }

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn get_parses_a_success_body() {
        let base = mock_server(http("200 OK", r#"{"value":[]}"#));
        let transport = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap();
        let doc = transport.get(&base).await.unwrap();
        assert!(doc.get("value").is_some());
    }

    #[tokio::test]
    async fn the_http_version_is_unknown_until_the_first_response() {
        // Graph's connect performs no I/O (no session discovery), so a fresh transport
        // has observed nothing; the first response fills the fact in. This is the one
        // provider where `ConnectionInfo::http_version` is `None` on a live connection.
        let base = mock_server(http("200 OK", r#"{"value":[]}"#));
        let transport = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap();
        assert_eq!(GraphTransport::http_version(&transport), None);

        transport.get(&base).await.unwrap();
        assert_eq!(
            GraphTransport::http_version(&transport),
            Some(HttpVersion::Http1_1)
        );
    }

    #[tokio::test]
    async fn get_bytes_returns_the_raw_body_verbatim() {
        // `$value` is not JSON — it is the raw RFC 822 MIME; get_bytes returns it as-is.
        let mime = "From: a@example.com\r\nSubject: Hi\r\n\r\nBody\r\n";
        let base = mock_server(http("200 OK", mime));
        let bytes = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .get_bytes(&base)
        .await
        .unwrap();
        assert_eq!(bytes, mime.as_bytes());
    }

    #[tokio::test]
    async fn get_bytes_non_success_is_a_classified_status_error() {
        // A gone/moved message → the same status classification as any Graph error.
        let body = r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#;
        let base = mock_server(http("404 Not Found", body));
        let err = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .get_bytes(&base)
        .await
        .unwrap_err();
        assert!(
            matches!(&err, GraphError::Status { code: Some(c), .. } if c == "ErrorItemNotFound")
        );
    }

    #[tokio::test]
    async fn non_success_status_becomes_a_classified_status_error() {
        let body = r#"{"error":{"code":"InvalidAuthenticationToken","message":"nope"}}"#;
        let base = mock_server(http("401 Unauthorized", body));
        let err = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .get(&base)
        .await
        .unwrap_err();
        assert!(
            matches!(&err, GraphError::Status { code: Some(c), .. } if c == "InvalidAuthenticationToken")
        );
        assert_eq!(err.failure_class(), FailureClass::Authentication);
    }

    #[tokio::test]
    async fn a_non_json_success_body_is_a_permanent_decode_error() {
        let base = mock_server(http("200 OK", "this is not json"));
        let err = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .get(&base)
        .await
        .unwrap_err();
        // A body that does not decode is a permanent protocol mismatch.
        assert!(matches!(err, GraphError::Transport(_)));
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }

    #[tokio::test]
    async fn a_refused_connection_is_a_retryable_transport_error() {
        // Nothing is listening on this port → reqwest connect error → retryable.
        let err = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .get("http://127.0.0.1:1/me")
        .await
        .unwrap_err();
        assert!(matches!(err, GraphError::Transport(_)));
        assert!(err.failure_class().is_retryable());
    }

    #[tokio::test]
    async fn a_write_body_parses_json_or_treats_empty_as_none() {
        // A create/patch echoes JSON; a 202/204 carries none.
        let base = mock_server(http("201 Created", r#"{"id":"x"}"#));
        let created = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .post(&base, "application/json", b"{}".to_vec())
        .await
        .unwrap();
        assert_eq!(created.unwrap()["id"], "x");

        let base = mock_server(http("204 No Content", ""));
        let none = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .patch(&base, "application/json", Some("etag"), b"{}".to_vec())
        .await
        .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn a_412_write_is_a_classified_conflict() {
        let body = r#"{"error":{"code":"ErrorIrresolvableConflict","message":"stale"}}"#;
        let base = mock_server(http("412 Precondition Failed", body));
        let err = HttpTransport::new(
            "tok".to_owned(),
            crate::test_support::tls(),
            crate::test_support::retry(),
        )
        .unwrap()
        .delete(&base, Some("stale-etag"))
        .await
        .unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }
}
