//! Offline test helpers: a fixture-routing fake [`GraphTransport`] so the fetch
//! and provider orchestration run against the captured real responses without
//! network. Shared by the `fetch` and `provider` test modules.

use std::sync::OnceLock;

use async_trait::async_trait;
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{
    error::GraphError,
    transport::{GraphClient, GraphTransport},
};

/// A shared bundled TLS config for tests that build a real transport. The offline
/// tests drive it over the plaintext replay server, so trust is never actually
/// exercised — this just satisfies the constructor.
pub(crate) fn tls() -> &'static TlsClientConfig {
    static TLS: OnceLock<TlsClientConfig> = OnceLock::new();
    TLS.get_or_init(TlsClientConfig::bundled)
}

/// What a fake route answers with: a fixture body, or an HTTP status plus the Graph error
/// envelope to fail with. The failing form exists so a recovery path can be driven offline —
/// notably Graph answering `410 SyncStateNotFound` once a stored deltaLink has aged out.
pub(crate) type FakeRoute = Result<Value, (u16, Value)>;

/// Returns the first routed answer whose key is a substring of the requested URL.
struct Fake {
    routes: Vec<(String, FakeRoute)>,
}

impl Fake {
    fn route(&self, url: &str) -> Result<&FakeRoute, GraphError> {
        self.routes
            .iter()
            .find(|(key, _)| url.contains(key.as_str()))
            .map(|(_, answer)| answer)
            .ok_or_else(|| GraphError::protocol(format!("no fake route for {url}")))
    }
}

#[async_trait]
impl GraphTransport for Fake {
    async fn get(&self, url: &str) -> Result<Value, GraphError> {
        match self.route(url)? {
            Ok(doc) => Ok(doc.clone()),
            Err((status, body)) => Err(GraphError::status(*status, body.to_string())),
        }
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        // A raw route carries its bytes as a JSON string (the `$value` MIME); anything
        // else is serialized back to JSON bytes so the seam stays uniform.
        match self.route(url)? {
            Ok(Value::String(mime)) => Ok(mime.clone().into_bytes()),
            Ok(other) => Ok(other.to_string().into_bytes()),
            Err((status, body)) => Err(GraphError::status(*status, body.to_string())),
        }
    }

    async fn post(
        &self,
        url: &str,
        _content_type: &str,
        _body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        // Like every offline fake, the request body is ignored — a matched route's canned
        // answer is served regardless of what was sent (`AGENTS.md`); the *request shape*
        // (valid base64 MIME, `text/plain`) is asserted by the mock-server transport test
        // and the live test. A route to `Value::Null` models a 202/204 no-body action.
        match self.route(url)? {
            Ok(Value::Null) => Ok(None),
            Ok(doc) => Ok(Some(doc.clone())),
            Err((status, body)) => Err(GraphError::status(*status, body.to_string())),
        }
    }
}

/// Builds a [`GraphClient`] backed by URL-substring → fixture routes.
pub(crate) fn fake_client(routes: Vec<(&str, Value)>) -> GraphClient {
    fake_client_fallible(
        routes
            .into_iter()
            .map(|(key, doc)| (key, Ok(doc)))
            .collect(),
    )
}

/// Builds a [`GraphClient`] whose routes may *fail* with an HTTP status, so an error-recovery
/// path is drivable without a live server (see [`FakeRoute`]).
pub(crate) fn fake_client_fallible(routes: Vec<(&str, FakeRoute)>) -> GraphClient {
    let routes = routes
        .into_iter()
        .map(|(key, answer)| (key.to_owned(), answer))
        .collect();
    GraphClient::with_transport(Box::new(Fake { routes }), "https://graph.test".to_owned())
}

/// Parses a fixture string into JSON.
pub(crate) fn json(fixture: &str) -> Value {
    serde_json::from_str(fixture).unwrap()
}

/// Spawns a deterministic fixture-replay HTTP server and returns its base URL.
///
/// Serves the first routed fixture whose key is a substring of the request path
/// (404 otherwise), over real HTTP — so a `GraphClient::with_base` drives the whole
/// stack (reqwest transport + URL rebasing + fetch orchestration) end-to-end in CI
/// without a live token. Routes are matched in order, so list the most specific
/// first. The background thread serves connections for the test's lifetime.
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
/// of what the real reqwest transport actually sent (`POST`, `Content-Type`, the
/// base64 MIME body) without a live token.
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
        // Stop once the headers and exactly `Content-Length` more body bytes are in.
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

/// Decodes a standard RFC 4648 base64 string (test-only; the crate itself only
/// encodes), stopping at the first `=` padding — enough to read back a captured
/// `sendMail` MIME body.
pub(crate) fn base64_decode(text: &str) -> Vec<u8> {
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in text.trim().as_bytes() {
        if byte == b'=' {
            break;
        }
        let Some(v) = value(byte) else { continue };
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).expect("masked to a byte"));
        }
    }
    out
}

/// The routes for a full folder-list sync: the `msgfolderroot` + six well-known
/// role aliases + the folder list.
pub(crate) fn folder_routes() -> Vec<(&'static str, Value)> {
    vec![
        (
            "/mailFolders/msgfolderroot",
            json(include_str!(
                "../tests/fixtures/wellknown/msgfolderroot.json"
            )),
        ),
        (
            "/mailFolders/inbox",
            json(include_str!("../tests/fixtures/wellknown/inbox.json")),
        ),
        (
            "/mailFolders/archive",
            json(include_str!("../tests/fixtures/wellknown/archive.json")),
        ),
        (
            "/mailFolders/drafts",
            json(include_str!("../tests/fixtures/wellknown/drafts.json")),
        ),
        (
            "/mailFolders/sentitems",
            json(include_str!("../tests/fixtures/wellknown/sentitems.json")),
        ),
        (
            "/mailFolders/deleteditems",
            json(include_str!(
                "../tests/fixtures/wellknown/deleteditems.json"
            )),
        ),
        (
            "/mailFolders/junkemail",
            json(include_str!("../tests/fixtures/wellknown/junkemail.json")),
        ),
        (
            "/mailFolders?$top",
            json(include_str!("../tests/fixtures/mail/mailfolders.json")),
        ),
    ]
}
