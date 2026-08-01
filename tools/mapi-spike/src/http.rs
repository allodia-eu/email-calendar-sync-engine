//! The MAPI/HTTP transport: [MS-OXCMAPIHTTP] §2.2.2 (POST format), §2.2.3
//! (headers), §2.2.7 (meta-tags).
//!
//! Two things here were learned from live servers and are not in the spec's
//! mandatory-header list (§2.2.2.1):
//!
//! * **`X-ClientInfo` is required *by Gromox*.** Omitting it makes Gromox reject every request
//!   before it looks at the body. **Exchange does not require it** (measured: `X-ResponseCode: 0`
//!   without it), so this is a vendor quirk, not a protocol rule. It is sent unconditionally
//!   because Outlook does — but a client must not treat its absence as fatal.
//! * **`/mapi/emsmdb/` requires the `?MailboxId=<guid>@<domain>` query parameter**, on both
//!   servers. Gromox 404s at the router; Exchange returns **HTTP 400 with no `X-ResponseCode` at
//!   all**, so a client keying only on that header sees "header absent" rather than a code. Either
//!   way it looks like "MAPI is not enabled" rather than "your URL is incomplete". Autodiscover
//!   hands over the full URL; use it verbatim.
//!
//! **`X-ResponseCode` values are not portable.** Exchange matches the spec table exactly
//! (`MissingHeader` = 7, `ContextNotFound` = 10, `InvalidRequestType` = 5, per
//! [MS-OXCMAPIHTTP] §2.2.3.3.3). Gromox returns 3 / 6 / 5 for conditions whose own diagnostic text
//! is the spec's wording for 7 / 13 / 12. So the code is reported raw and the body text is
//! preserved: a client that hard-maps codes by the spec table would misreport Gromox, and one that
//! hard-maps them by Gromox's would misreport Exchange.

use std::{fmt, time::Duration};

use crate::cursor::Reader;

/// A session cookie jar. `reqwest`'s cookie store is not enabled in this
/// workspace's feature set, and the protocol only needs echo-everything
/// behaviour, so this is deliberately ~20 lines rather than a dependency.
#[derive(Debug, Default)]
pub struct Cookies {
    jar: Vec<(String, String)>,
}

impl Cookies {
    pub fn absorb(&mut self, set_cookie: &str) {
        let pair = set_cookie.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else {
            return;
        };
        let name = name.trim().to_owned();
        let value = value.trim().to_owned();
        if let Some(slot) = self.jar.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value;
        } else {
            self.jar.push((name, value));
        }
    }

    pub fn header(&self) -> Option<String> {
        if self.jar.is_empty() {
            return None;
        }
        Some(
            self.jar
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.jar.is_empty()
    }
}

/// A parsed MAPI/HTTP response: the transport-level code, the in-body
/// meta-tags and additional headers, and the binary response body.
#[derive(Debug)]
pub struct Response {
    pub response_code: u32,
    pub meta_tags: Vec<String>,
    pub additional_headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The whole HTTP payload as received, kept for transcript capture.
    pub raw: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        self.response_code == 0
    }

    /// The diagnostic text Gromox puts in its HTML error page, if any.
    pub fn diagnostic(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.raw);
        let start = text.find("<p>")? + 3;
        let end = text[start..].find("</p>")? + start;
        Some(text[start..end].to_owned())
    }

    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.body)
    }
}

#[derive(Debug)]
pub enum Error {
    Http(String),
    /// Transport-level failure: `X-ResponseCode` was non-zero.
    ResponseCode {
        code: u32,
        diagnostic: Option<String>,
    },
    MissingResponseCode,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(e) => write!(f, "http error: {e}"),
            Self::ResponseCode { code, diagnostic } => match diagnostic {
                Some(d) => write!(f, "X-ResponseCode {code}: {d}"),
                None => write!(f, "X-ResponseCode {code}"),
            },
            Self::MissingResponseCode => {
                write!(f, "response carried no X-ResponseCode header")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// One MAPI/HTTP Session Context.
pub struct Session {
    client: reqwest::blocking::Client,
    endpoint: String,
    user: String,
    pass: String,
    cookies: Cookies,
    /// [MS-OXCMAPIHTTP] §2.2.3.3.2: the GUID must not change for the life of
    /// the Session Context; the counter must increase on every request.
    request_guid: String,
    counter: u64,
    client_info: String,
    /// When set, every exchange is written out as a byte pair. The recorder
    /// never sees the `Authorization` header, so credentials cannot reach a
    /// capture by omission of a scrubbing step.
    recorder: Option<crate::transcript::Recorder>,
    scrub: Vec<(String, String)>,
}

impl Session {
    pub fn new(
        endpoint: impl Into<String>,
        user: impl Into<String>,
        pass: impl Into<String>,
    ) -> Self {
        Self::with_tls(endpoint, user, pass, false)
    }

    /// `insecure` skips certificate validation. Exchange Server requires SSL on
    /// the MAPI virtual directory and a lab install ships a self-signed cert, so
    /// a spike needs this to talk to one at all. It is a spike-only affordance:
    /// a real `provider-mapi` would take an `engine_tls::TlsClientConfig` and
    /// have no such switch.
    pub fn with_tls(
        endpoint: impl Into<String>,
        user: impl Into<String>,
        pass: impl Into<String>,
        insecure: bool,
    ) -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(insecure)
                .danger_accept_invalid_hostnames(insecure)
                .build()
                .expect("blocking client"),
            endpoint: endpoint.into(),
            user: user.into(),
            pass: pass.into(),
            cookies: Cookies::default(),
            request_guid: "{12345678-1234-1234-1234-123456789abc}".into(),
            counter: 0,
            client_info: "{2EF33C39-49C8-421C-B876-CDF7F2AC3AA0}:123".into(),
            recorder: None,
            scrub: Vec::new(),
        }
    }

    /// Capture every exchange under `dir`, rewriting each `(needle, replacement)`
    /// pair on the way out.
    pub fn record_to(
        &mut self,
        dir: &str,
        scrub: Vec<(String, String)>,
    ) -> std::io::Result<&std::path::Path> {
        self.recorder = Some(crate::transcript::Recorder::new(dir)?);
        self.scrub = scrub;
        Ok(self.recorder.as_ref().expect("just set").dir())
    }

    pub fn has_session(&self) -> bool {
        !self.cookies.is_empty()
    }

    /// POST one request type with a binary body.
    pub fn post(&mut self, request_type: &str, body: Vec<u8>) -> Result<Response> {
        self.counter += 1;
        let recorded_request = self.recorder.is_some().then(|| body.clone());
        let mut req = self
            .client
            .post(&self.endpoint)
            .basic_auth(&self.user, Some(&self.pass))
            .header("Content-Type", "application/mapi-http")
            .header("X-RequestType", request_type)
            .header(
                "X-RequestId",
                format!("{}:{}", self.request_guid, self.counter),
            )
            .header("X-ClientApplication", "Outlook/15.00.0847.4040")
            .header("X-ClientInfo", &self.client_info)
            .body(body);

        if let Some(cookie) = self.cookies.header() {
            req = req.header("Cookie", cookie);
        }

        let resp = req.send().map_err(|e| Error::Http(e.to_string()))?;

        let code = resp
            .headers()
            .get("X-ResponseCode")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u32>().ok());

        for value in resp.headers().get_all("Set-Cookie") {
            if let Ok(v) = value.to_str() {
                self.cookies.absorb(v);
            }
        }

        let status = resp.status();
        let raw = resp
            .bytes()
            .map_err(|e| Error::Http(e.to_string()))?
            .to_vec();

        // Capture before any early return, so a *failed* exchange is recorded
        // too — those are the transcripts worth the most to the next reader.
        if let (Some(rec), Some(request)) = (self.recorder.as_mut(), recorded_request) {
            let notes = vec![
                ("HTTP-Status".to_owned(), status.as_u16().to_string()),
                (
                    "X-ResponseCode".to_owned(),
                    code.map_or_else(|| "<absent>".to_owned(), |c| c.to_string()),
                ),
            ];
            let rules: Vec<(&str, &str)> = self
                .scrub
                .iter()
                .map(|(n, r)| (n.as_str(), r.as_str()))
                .collect();
            if let Err(e) = rec.record(request_type, &request, &raw, &notes, &rules) {
                eprintln!("warning: could not write transcript: {e}");
            }
        }

        let Some(code) = code else {
            return Err(Error::MissingResponseCode);
        };

        let parsed = parse_payload(&raw);
        let out = Response {
            response_code: code,
            meta_tags: parsed.0,
            additional_headers: parsed.1,
            body: parsed.2,
            raw,
        };

        if !out.ok() {
            return Err(Error::ResponseCode {
                code,
                diagnostic: out.diagnostic(),
            });
        }
        Ok(out)
    }
}

/// Split the inner response stream ([MS-OXCMAPIHTTP] §2.2.7): CRLF-delimited
/// ASCII meta-tag lines (`PROCESSING`, `PENDING`, then `DONE`), then
/// `Key: Value` additional headers, then a blank line, then the binary body.
///
/// This runs *after* HTTP chunked decoding, which reqwest does transparently.
/// It is a parser over untrusted bytes, so it must never panic: a payload with
/// no `DONE` yields no body rather than an error, and the body is returned
/// byte-exact (never through `from_utf8_lossy`, which would corrupt a
/// `RopBuffer`).
fn parse_payload(raw: &[u8]) -> (Vec<String>, Vec<(String, String)>, Vec<u8>) {
    let mut meta_tags = Vec::new();
    let mut headers = Vec::new();
    let mut pos = 0usize;
    let mut done = false;

    while pos < raw.len() {
        // No line terminator left. If we never saw DONE there was no meta-tag
        // preamble at all (Gromox's non-chunked HTML error pages), so the whole
        // payload is the body.
        let Some(eol) = find_crlf(&raw[pos..]) else {
            return if done {
                (meta_tags, headers, raw[pos..].to_vec())
            } else {
                (meta_tags, headers, raw.to_vec())
            };
        };
        let line = String::from_utf8_lossy(&raw[pos..pos + eol])
            .trim()
            .to_owned();
        pos += eol + 2;

        if !done {
            match line.to_ascii_uppercase().as_str() {
                "PROCESSING" | "PENDING" => meta_tags.push(line),
                "DONE" => {
                    meta_tags.push(line);
                    done = true;
                }
                // Not a meta-tag: this payload has no meta-tag preamble at all
                // (Gromox's non-chunked error pages, for one). Treat the whole
                // thing as the body.
                _ => return (meta_tags, headers, raw.to_vec()),
            }
            continue;
        }

        if line.is_empty() {
            return (meta_tags, headers, raw[pos..].to_vec());
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }

    (meta_tags, headers, Vec::new())
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_meta_tags_headers_and_binary_body() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"PROCESSING\r\nPENDING\r\nDONE\r\n");
        raw.extend_from_slice(b"X-ElapsedTime: 12\r\nX-StartTime: now\r\n\r\n");
        // A body that is deliberately NOT valid UTF-8 — a RopBuffer never is.
        raw.extend_from_slice(&[0x00, 0xFF, 0xFE, 0x80, 0x01]);

        let (tags, headers, body) = parse_payload(&raw);
        assert_eq!(tags, vec!["PROCESSING", "PENDING", "DONE"]);
        assert_eq!(headers[0], ("X-ElapsedTime".into(), "12".into()));
        assert_eq!(headers[1], ("X-StartTime".into(), "now".into()));
        // Byte-exact: this is the whole reason the body is Vec<u8>, not String.
        assert_eq!(body, vec![0x00, 0xFF, 0xFE, 0x80, 0x01]);
    }

    #[test]
    fn payload_with_no_meta_tags_is_all_body() {
        let raw = b"<html>error page</html>";
        let (tags, _, body) = parse_payload(raw);
        assert!(tags.is_empty());
        assert_eq!(body, raw.to_vec());
    }

    #[test]
    fn done_with_no_additional_headers() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"DONE\r\n\r\n");
        raw.extend_from_slice(&[0xAA, 0xBB]);
        let (tags, headers, body) = parse_payload(&raw);
        assert_eq!(tags, vec!["DONE"]);
        assert!(headers.is_empty());
        assert_eq!(body, vec![0xAA, 0xBB]);
    }

    #[test]
    fn truncated_payloads_never_panic() {
        for raw in [
            &b""[..],
            &b"DONE"[..],           // no CRLF
            &b"DONE\r\n"[..],       // no blank line
            &b"PROCESSING\r\n"[..], // never completes
            &[0xFF, 0xFE, 0x00][..],
        ] {
            let _ = parse_payload(raw);
        }
    }

    #[test]
    fn cookie_jar_replaces_rather_than_appends() {
        let mut c = Cookies::default();
        c.absorb("sid=abc; Path=/; HttpOnly");
        assert_eq!(c.header().unwrap(), "sid=abc");
        c.absorb("sid=def; Path=/");
        assert_eq!(c.header().unwrap(), "sid=def");
        c.absorb("other=1");
        assert_eq!(c.header().unwrap(), "sid=def; other=1");
    }

    #[test]
    fn malformed_set_cookie_is_ignored() {
        let mut c = Cookies::default();
        c.absorb("no-equals-sign");
        assert!(c.is_empty());
    }
}
