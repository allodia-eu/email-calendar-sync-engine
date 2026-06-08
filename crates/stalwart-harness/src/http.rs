//! Minimal HTTP/1.0 client for the smoke probes.
//!
//! Deliberately tiny and dependency-free: HTTP/1.0 with `Connection: close`
//! means the server closes the socket after the response, so reading to EOF
//! yields the whole body with no chunked-transfer framing to decode. This is
//! enough to poll readiness and to exercise the JMAP and CalDAV endpoints; it is
//! not a general-purpose HTTP client.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::HarnessError;

/// Timeout applied to connect, read, and write for every probe socket.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A parsed HTTP response: status code plus the raw header block and body.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The numeric status code from the response line (e.g. `200`, `207`).
    pub status: u16,
    /// The raw header block, verbatim, without the trailing blank line.
    pub headers: String,
    /// The response body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The body decoded as UTF-8 lossily, for substring assertions in tests.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Whether the body contains `needle` (UTF-8 lossy).
    #[must_use]
    pub fn body_contains(&self, needle: &str) -> bool {
        self.body_text().contains(needle)
    }
}

/// Send one HTTP/1.0 request over a fresh plaintext connection and read the
/// whole response.
///
/// `auth` is an optional `(user, password)` pair sent as HTTP Basic. `headers`
/// are extra request headers (e.g. `Depth` for a CalDAV `PROPFIND`).
///
/// # Errors
///
/// Returns [`HarnessError::Io`] if the address cannot be resolved or the socket
/// read/write fails, and [`HarnessError::Protocol`] if the response is not a
/// well-formed HTTP status line.
pub(crate) fn request(
    addr: &str,
    method: &str,
    path: &str,
    auth: Option<(&str, &str)>,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<HttpResponse, HarnessError> {
    let io = |source| HarnessError::Io {
        addr: addr.to_owned(),
        source,
    };

    let socket =
        addr.to_socket_addrs()
            .map_err(io)?
            .next()
            .ok_or_else(|| HarnessError::Protocol {
                protocol: "http",
                detail: format!("no address resolved for {addr}"),
            })?;
    let mut stream = TcpStream::connect_timeout(&socket, IO_TIMEOUT).map_err(io)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;

    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\n");
    if let Some((user, pass)) = auth {
        let token = base64_encode(format!("{user}:{pass}").as_bytes());
        req.push_str("Authorization: Basic ");
        req.push_str(&token);
        req.push_str("\r\n");
    }
    for (name, value) in headers {
        req.push_str(name);
        req.push_str(": ");
        req.push_str(value);
        req.push_str("\r\n");
    }
    if !body.is_empty() {
        req.push_str("Content-Length: ");
        req.push_str(&body.len().to_string());
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");

    stream.write_all(req.as_bytes()).map_err(io)?;
    stream.write_all(body).map_err(io)?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(io)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, HarnessError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HarnessError::Protocol {
            protocol: "http",
            detail: "no header/body delimiter in response".to_owned(),
        })?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HarnessError::Protocol {
            protocol: "http",
            detail: format!("malformed status line: {status_line:?}"),
        })?;
    Ok(HttpResponse {
        status,
        headers: lines.collect::<Vec<_>>().join("\r\n"),
        body: raw[split + 4..].to_vec(),
    })
}

/// Standard base64 (RFC 4648) with padding.
#[must_use]
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[((n >> 18) & 63) as usize]));
        out.push(char::from(TABLE[((n >> 12) & 63) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[((n >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[(n & 63) as usize])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, parse_response};

    // RFC 4648 test vectors.
    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_basic_auth_pair() {
        assert_eq!(base64_encode(b"admin:secret"), "YWRtaW46c2VjcmV0");
    }

    #[test]
    fn parse_response_splits_status_headers_body() {
        let raw =
            b"HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\n\r\n<d:multistatus/>";
        let resp = parse_response(raw).expect("parses");
        assert_eq!(resp.status, 207);
        assert!(resp.headers.contains("Content-Type: application/xml"));
        assert_eq!(resp.body, b"<d:multistatus/>");
    }

    #[test]
    fn parse_response_rejects_garbage() {
        assert!(parse_response(b"not http").is_err());
    }
}
