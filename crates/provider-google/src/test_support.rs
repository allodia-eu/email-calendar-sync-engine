//! Offline test helpers: a fixture-routing fake [`GoogleTransport`] so the fetch and
//! provider orchestration run against captured real responses without network, plus a
//! fixture-replay HTTP server and a request-capturing server (for asserting write
//! request *shapes*, which the fakes ignore — `AGENTS.md`).

use std::sync::OnceLock;

use async_trait::async_trait;
use engine_provider::HttpVersion;
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{
    error::GoogleError,
    transport::{GoogleClient, GoogleTransport},
};

/// A shared bundled TLS config for tests that build a real transport. The offline
/// tests drive it over the plaintext replay server, so trust is never actually
/// exercised — this just satisfies the constructor.
pub(crate) fn tls() -> &'static TlsClientConfig {
    static TLS: OnceLock<TlsClientConfig> = OnceLock::new();
    TLS.get_or_init(TlsClientConfig::bundled)
}

/// What a fake route answers with: a fixture body, or an HTTP status plus the Google
/// error envelope to fail with. The failing form exists so a recovery path can be
/// driven offline — notably Google Calendar answering `410 fullSyncRequired` or Gmail
/// answering `404` once a stored cursor has aged out.
pub(crate) type FakeRoute = Result<Value, (u16, Value)>;

/// Returns the first routed answer whose key is a substring of the requested URL.
struct Fake {
    routes: Vec<(String, FakeRoute)>,
}

impl Fake {
    fn route(&self, url: &str) -> Result<&FakeRoute, GoogleError> {
        self.routes
            .iter()
            .find(|(key, _)| url.contains(key.as_str()))
            .map(|(_, answer)| answer)
            .ok_or_else(|| GoogleError::protocol(format!("no fake route for {url}")))
    }
}

#[async_trait]
impl GoogleTransport for Fake {
    async fn get(&self, url: &str) -> Result<Value, GoogleError> {
        match self.route(url)? {
            Ok(doc) => Ok(doc.clone()),
            Err((status, body)) => Err(GoogleError::status(*status, body.to_string())),
        }
    }

    async fn post(
        &self,
        url: &str,
        _content_type: &str,
        _body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        // Like every offline fake, the request body is ignored — a matched route's
        // canned answer is served regardless of what was sent (`AGENTS.md`); the
        // *request shape* is asserted by the capturing-server tests and the live tests.
        // A route to `Value::Null` models a no-body (204) action.
        match self.route(url)? {
            Ok(Value::Null) => Ok(None),
            Ok(doc) => Ok(Some(doc.clone())),
            Err((status, body)) => Err(GoogleError::status(*status, body.to_string())),
        }
    }

    async fn patch(
        &self,
        url: &str,
        _content_type: &str,
        _if_match: Option<&str>,
        _body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        // Body/If-Match ignored (canned answer, `AGENTS.md`); the request shape is
        // asserted by the capturing-server tests and the live tests.
        match self.route(url)? {
            Ok(Value::Null) => Ok(None),
            Ok(doc) => Ok(Some(doc.clone())),
            Err((status, body)) => Err(GoogleError::status(*status, body.to_string())),
        }
    }

    async fn delete(&self, url: &str, _if_match: Option<&str>) -> Result<(), GoogleError> {
        match self.route(url)? {
            Ok(_) => Ok(()),
            Err((status, body)) => Err(GoogleError::status(*status, body.to_string())),
        }
    }

    fn http_version(&self) -> Option<HttpVersion> {
        // A fake never speaks HTTP, but reporting a version lets the provider's
        // connection_info be exercised offline.
        Some(HttpVersion::Http2)
    }
}

/// Builds a [`GoogleClient`] backed by URL-substring → fixture routes.
pub(crate) fn fake_client(routes: Vec<(&str, Value)>) -> GoogleClient {
    fake_client_fallible(
        routes
            .into_iter()
            .map(|(key, doc)| (key, Ok(doc)))
            .collect(),
    )
}

/// Builds a [`GoogleClient`] whose routes may *fail* with an HTTP status, so an
/// error-recovery path is drivable without a live server (see [`FakeRoute`]).
pub(crate) fn fake_client_fallible(routes: Vec<(&str, FakeRoute)>) -> GoogleClient {
    let routes = routes
        .into_iter()
        .map(|(key, answer)| (key.to_owned(), answer))
        .collect();
    GoogleClient::with_transport(Box::new(Fake { routes }), "https://google.test".to_owned())
}

/// Parses a fixture string into JSON.
pub(crate) fn json(fixture: &str) -> Value {
    serde_json::from_str(fixture).unwrap()
}

/// Spawns a deterministic fixture-replay HTTP server and returns its base URL.
///
/// Serves the first routed fixture whose key is a substring of the request path (404
/// otherwise), over real HTTP — so a `GoogleClient::with_base` drives the whole stack
/// (reqwest transport + fetch orchestration) end-to-end in CI without a live token.
/// Routes are matched in order, so list the most specific first. The background thread
/// serves connections for the test's lifetime.
pub(crate) fn replay_server(routes: Vec<(&'static str, Value)>) -> String {
    use std::io::{Read, Write};
    let routes: Vec<(String, String)> = routes
        .into_iter()
        .map(|(key, doc)| (key.to_owned(), doc.to_string()))
        .collect();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            let response = match routes.iter().find(|(key, _)| path.contains(key.as_str())) {
                Some((_, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

/// Spawns a one-shot HTTP server that **captures** the full request (headers + body,
/// read to `Content-Length`) and answers with `status`/`body`, returning its base URL
/// and a receiver for the captured request text.
///
/// The fixture-routing [`Fake`] and [`replay_server`] ignore the request body (like
/// every offline fake — `AGENTS.md`), so this is how a write test asserts the *shape*
/// of what the real reqwest transport actually sent (`POST`, `Content-Type`, the JSON
/// body, the base64url MIME) without a live token.
pub(crate) fn capturing_server(
    status: &str,
    body: &str,
) -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let request = read_full_request(&mut stream);
            let _ = tx.send(request);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), rx)
}

/// Reads a full HTTP request (headers + `Content-Length` body) off `stream`.
fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if request_complete(&data) {
            break;
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// Whether `data` holds a complete HTTP request: the header terminator plus at least
/// the `Content-Length` body bytes that follow it.
fn request_complete(data: &[u8]) -> bool {
    let Some(header_end) = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
    else {
        return false;
    };
    let headers = String::from_utf8_lossy(&data[..header_end]);
    let len = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    data.len() >= header_end + len
}
