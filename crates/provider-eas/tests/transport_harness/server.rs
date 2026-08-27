// SPDX-License-Identifier: MPL-2.0
//! The mock EAS server: plain-HTTP and TLS listeners on 127.0.0.1 that
//! capture every request and answer from a per-request handler.
//!
//! Mirrors `provider-graph`'s `replay_server`/`capturing_server` (one
//! background `std::thread` per listener, hand-rolled HTTP/1.1 with
//! `Connection: close` so each request arrives on its own connection —
//! reqwest simply reconnects), extended with a handler closure so a whole
//! retry/provision/redirect sequence can be scripted by inspecting the
//! request. The TLS variant follows `engine-tls`'s in-process TLS server
//! precedent (`rcgen` self-signed cert + rustls `StreamOwned` over a
//! std `TcpStream`).

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
};

use provider_eas::wbxml::{WbxmlElement, deserialize_to_tree, serialize_tree};

/// One captured HTTP request, as the client actually sent it.
#[derive(Debug, Clone)]
pub(crate) struct CapturedRequest {
    /// Request method (`POST`, `OPTIONS`, `GET`).
    pub(crate) method: String,
    /// Path without the query string (e.g. `/Microsoft-Server-ActiveSync`).
    pub(crate) path: String,
    /// Raw query string, without the `?` (e.g. `Cmd=Sync&User=…`).
    pub(crate) query: String,
    /// Header names lowercased, in wire order.
    pub(crate) headers: Vec<(String, String)>,
    /// Body bytes (empty when no body).
    pub(crate) body: Vec<u8>,
}

impl CapturedRequest {
    /// First header value for `name` (case-insensitive).
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == lower)
            .map(|(_, v)| v.as_str())
    }

    /// The EAS `Cmd` query parameter — the command name every scenario
    /// routes on.
    pub(crate) fn cmd(&self) -> Option<String> {
        self.query
            .split('&')
            .find_map(|pair| pair.strip_prefix("Cmd=").map(|c| c.replace("%20", " ")))
    }

    /// The `X-MS-PolicyKey` header (the provision scenario's core
    /// assertion), defaulting to `"0"` the way the client sends it.
    pub(crate) fn policy_key(&self) -> &str {
        self.header("x-ms-policykey").unwrap_or("0")
    }

    /// The decoded WBXML request body (`None` when the body is not WBXML or
    /// empty — e.g. OPTIONS, autodiscover XML).
    pub(crate) fn wbxml_tree(&self) -> Option<WbxmlElement> {
        if self.body.is_empty() {
            return None;
        }
        deserialize_to_tree(&self.body).ok()
    }

    /// Body as lossy UTF-8 (autodiscover POX envelopes, error pages).
    pub(crate) fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// What the mock answers with. Body builders cover every wire form the
/// client understands: WBXML, empty (SendMail success), XML/JSON/text, and
/// raw multipart bytes.
#[derive(Debug, Clone)]
pub(crate) struct MockResponse {
    /// HTTP status code.
    pub(crate) status: u16,
    /// Header pairs, wire order.
    pub(crate) headers: Vec<(String, String)>,
    /// Body bytes.
    pub(crate) body: Vec<u8>,
}

/// The WBXML content type per [MS-ASHTTP] §2.2.1.1.2.1.
const CT_WBXML: &str = "application/vnd.ms-sync.wbxml";
/// The multipart content type per [MS-ASCMD] §2.2.1.10.1.
const CT_MULTIPART: &str = "application/vnd.ms-sync.multipart";

impl MockResponse {
    /// `200` + WBXML body — the normal command answer.
    pub(crate) fn wbxml(tree: &WbxmlElement) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), CT_WBXML.into())],
            body: serialize_tree(tree).expect("fixture tree serializes"),
        }
    }

    /// `200` + WBXML content type + EMPTY body — the SendMail-family
    /// success shape ([MS-ASCMD] §2.2.1.13).
    pub(crate) fn empty_wbxml() -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), CT_WBXML.into())],
            body: Vec::new(),
        }
    }

    /// `200` + a multipart envelope body — the opted-in ItemOperations form.
    pub(crate) fn multipart(parts: &[Vec<u8>]) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), CT_MULTIPART.into())],
            body: multipart_body(parts),
        }
    }

    /// Arbitrary status + content type + body (error pages, POX XML, JSON).
    pub(crate) fn raw(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
        }
    }

    /// A status-only answer with no body and no content type (451/429/449
    /// often carry nothing but headers).
    pub(crate) fn bare(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Add (or replace) a header — `Retry-After`, `X-MS-Location`,
    /// `MS-ASProtocolVersions`, ….
    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// Build a [MS-ASCMD] §2.2.1.10.1.1 MultiPartResponse envelope:
/// `PartCount u32 LE` + per-part `(offset u32 LE, length u32 LE)` + the parts.
pub(crate) fn multipart_body(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        &u32::try_from(parts.len())
            .expect("part count fits u32")
            .to_le_bytes(),
    );
    let data_start = 4 + parts.len() * 8;
    let mut offset = data_start;
    for part in parts {
        let length = u32::try_from(part.len()).expect("part len fits u32");
        bytes.extend_from_slice(
            &u32::try_from(offset)
                .expect("part offset fits u32")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&length.to_le_bytes());
        offset += part.len();
    }
    for part in parts {
        bytes.extend_from_slice(part);
    }
    bytes
}

/// Per-request handler: receives the request and its 1-based ordinal (the
/// Nth request this server has served) and decides the answer.
pub(crate) type Handler = Arc<dyn Fn(&CapturedRequest, usize) -> MockResponse + Send + Sync>;

/// A running mock server. The background thread serves connections until
/// dropped-and-detached (the process ends with the test — the
/// `provider-graph` replay-server convention).
pub(crate) struct MockServer {
    /// Base URL (`http://127.0.0.1:PORT` or `https://…`).
    pub(crate) base_url: String,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl MockServer {
    /// Plain-HTTP server: every command scenario except the 451 redirect and
    /// autodiscover, which need TLS.
    pub(crate) fn http(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server port");
        let addr = listener.local_addr().expect("mock server address");
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                serve_one(&mut stream, &handler, &sink);
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            captured,
        }
    }

    /// TLS server with a freshly minted self-signed cert (the
    /// `engine-tls` in-process TLS test precedent). The client must be built
    /// with `TlsClientConfig::dangerous_accept_any()` to accept it.
    pub(crate) fn https(handler: Handler) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("mint mock server cert");
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .expect("mock server cert loads");
        // Offer http/1.1 only — the client's h2-capable ALPN list falls back
        // to it, and the hand-rolled response writer speaks HTTP/1.1.
        let mut config = config;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock TLS port");
        let addr = listener.local_addr().expect("mock TLS address");
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&config)) else {
                    continue;
                };
                let mut stream = rustls::StreamOwned::new(conn, stream);
                serve_one(&mut stream, &handler, &sink);
            }
        });
        Self {
            base_url: format!("https://{addr}"),
            captured,
        }
    }

    /// The EAS endpoint URL under this server ([MS-ASHTTP] §2.1 fixed path).
    pub(crate) fn eas_url(&self) -> String {
        format!("{}/Microsoft-Server-ActiveSync", self.base_url)
    }

    /// Every request served so far, in order.
    pub(crate) fn captured(&self) -> Vec<CapturedRequest> {
        self.captured.lock().expect("capture lock").clone()
    }

    /// The Nth (1-based) captured request.
    pub(crate) fn request(&self, ordinal: usize) -> CapturedRequest {
        self.captured()
            .into_iter()
            .nth(ordinal - 1)
            .unwrap_or_else(|| panic!("request {ordinal} never arrived"))
    }

    /// How many requests have been served.
    pub(crate) fn count(&self) -> usize {
        self.captured.lock().expect("capture lock").len()
    }

    /// Wait (bounded) until `n` requests have been captured — the TLS and
    /// ping scenarios answer asynchronously from the server thread.
    pub(crate) fn await_count(&self, n: usize) {
        for _ in 0..200 {
            if self.count() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("expected {n} requests, saw {} after 2s", self.count());
    }
}

/// Read exactly one HTTP request off `stream`, capture it, answer, close.
fn serve_one(stream: &mut impl ReadWrite, handler: &Handler, sink: &Mutex<Vec<CapturedRequest>>) {
    let Some(request) = read_request(stream) else {
        return;
    };
    let ordinal = {
        let mut guard = sink.lock().expect("capture lock");
        guard.push(request.clone());
        guard.len()
    };
    let response = handler(&request, ordinal);
    // A write failure means the client hung up mid-flight (e.g. a
    // per-request timeout fired); the scenario's own assertions decide
    // whether that was expected, so the error is not surfaced here.
    let _ = write_response(stream, &response);
}

/// Anything the HTTP layer can read and write — a plain `TcpStream` or a
/// rustls `StreamOwned`. Keeps one request loop for both transports.
pub(crate) trait ReadWrite: Read + Write {}
impl ReadWrite for TcpStream {}
impl ReadWrite for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {}

/// Read one full HTTP request: headers, then exactly `Content-Length` body
/// bytes (`provider-graph`'s `read_full_request`).
fn read_request(stream: &mut impl ReadWrite) -> Option<CapturedRequest> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&data) {
            break pos;
        }
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while data.len() < header_end + content_length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }

    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target, String::new()),
    };
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Some(CapturedRequest {
        method,
        path,
        query,
        headers,
        body: data[header_end..header_end + content_length].to_vec(),
    })
}

/// Byte offset just past the `\r\n\r\n` header terminator.
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// Write the response with `Content-Length` and `Connection: close`, then
/// let the stream drop (one request per connection).
fn write_response(stream: &mut impl ReadWrite, response: &MockResponse) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let reason = match response.status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        449 => "Retry After Provisioning",
        451 => "Redirect",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        // `write!` into the String (clippy pedantic: no format-push).
        let _ = write!(head, "{name}: {value}\r\n");
    }
    let _ = write!(head, "Content-Length: {}\r\n", response.body.len());
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}
