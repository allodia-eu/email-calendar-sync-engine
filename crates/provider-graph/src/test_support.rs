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
