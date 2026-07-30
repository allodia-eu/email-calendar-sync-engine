//! IMAP transport: the tagged line protocol over any async stream.
//!
//! [`Connection`] is generic over the stream `S`, so the offline tests drive the
//! whole protocol over an in-memory mock while the live client uses a `tokio-rustls`
//! TLS stream — command sequencing, literal handling, and parsing are identical in
//! both (`docs/agent-guidance/imap-smtp.md`). It speaks only the handful of commands
//! the engine needs; the vocabulary itself lives in [`crate::transport_commands`], and the
//! higher-level snapshot/delta logic in [`crate::sync`]. This file owns the framing: the
//! greeting, tags, `{n}` literals, the tagged request/response round trip, `LOGIN`, and the
//! post-auth capability negotiation.

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::{ImapError, ImapResult};

/// The largest `{n}` literal we will read into memory. A hostile or buggy server
/// could announce an enormous literal (`* {4000000000}`); the cap bounds the
/// allocation so adversarial input cannot exhaust memory (`north-star.md` security).
/// Generous enough for any real metadata response (and future body fetches).
const MAX_LITERAL: usize = 64 * 1024 * 1024;

/// A connected IMAP session over a generic async byte stream.
// The flags below are independent facts the post-auth `CAPABILITY` reported — one per
// optional extension — not the state of a state machine, so the excessive-bools
// heuristic's "use an enum" suggestion does not apply; each is queried on its own.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent negotiated-extension flags, not state-machine state"
)]
pub(crate) struct Connection<S> {
    pub(crate) inner: BufReader<S>,
    /// The command-tag counter (`a1`, `a2`, …); `pub(crate)` so
    /// [`Connection::resume`](crate::transport_starttls) can reset it on the
    /// post-STARTTLS stream.
    pub(crate) tag: u32,
    /// Whether QRESYNC (RFC 7162) was negotiated for this session — set by
    /// [`Connection::negotiate_qresync`]. When `true`, the sync layer opens mailboxes
    /// with CONDSTORE and reconciles deltas via `CHANGEDSINCE`/`VANISHED`. `pub(crate)`
    /// so [`Connection::resume`](crate::transport_starttls) can seed the post-STARTTLS
    /// defaults.
    pub(crate) qresync: bool,
    /// Whether the server advertised `IDLE` (RFC 2177) in its post-auth `CAPABILITY` —
    /// recorded by [`Connection::negotiate_qresync`] from the same response. When
    /// `true`, a [`crate::watch::ImapWatcher`] can keep a standing connection idling to
    /// push change notifications; when `false`, the host must fall back to polling.
    /// `pub(crate)` for the same reason as [`qresync`](Self::qresync).
    pub(crate) idle_advertised: bool,
    /// Whether the server advertised `NAMESPACE` (RFC 2342) post-auth. When `false` there
    /// is no way to tell whose mail a folder holds, so every mailbox is read as the
    /// credential's own — the behaviour that predates shared-mailbox support.
    pub(crate) namespace_advertised: bool,
    /// Whether the server advertised `ACL` (RFC 4314) post-auth. When `false`, `MYRIGHTS`
    /// is not issued at all and a mailbox's rights are reported as
    /// [`MailboxAccess::owner`](engine_core::mail::MailboxAccess::owner) — the only honest
    /// answer when the protocol offers no way to ask.
    pub(crate) acl_advertised: bool,
    /// The tag of a streamed `UID FETCH` ([`Connection::uid_fetch_stream_start`]) whose
    /// tagged completion has not yet been read — set while its rows are being pulled
    /// one at a time. If a streaming fetch is **abandoned** mid-response (the caller
    /// drops its stream on a `StaleLease` restart), this stays set; the next
    /// [`Connection::command`] drains the leftover response to this tag before issuing
    /// its own, so the connection self-heals rather than desyncing.
    pub(crate) pending_tag: Option<String>,
}

impl<S> core::fmt::Debug for Connection<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Connection")
            .field("tag", &self.tag)
            .field("qresync", &self.qresync)
            .field("idle_advertised", &self.idle_advertised)
            .field("namespace_advertised", &self.namespace_advertised)
            .field("acl_advertised", &self.acl_advertised)
            .finish_non_exhaustive()
    }
}

impl<S> Connection<S> {
    /// Whether the server advertised `IDLE` (RFC 2177) post-auth — the precondition a
    /// [`crate::watch::ImapWatcher`] checks before opening a standing IDLE session, and
    /// what [`ImapProvider::build`](crate::provider) reads to advertise
    /// [`Capabilities::idle`](engine_provider::Capabilities::idle). A plain field read,
    /// so it needs no stream bounds (the unbounded provider builder consults it).
    pub(crate) fn idle_advertised(&self) -> bool {
        self.idle_advertised
    }

    /// Whether the server advertised `NAMESPACE` (RFC 2342) post-auth — the precondition
    /// for telling the credential's own mailboxes from ones shared with it. A plain field
    /// read, like [`idle_advertised`](Self::idle_advertised).
    pub(crate) fn namespace_advertised(&self) -> bool {
        self.namespace_advertised
    }

    /// Whether the server advertised `ACL` (RFC 4314) post-auth — the precondition for
    /// asking what may be done in a mailbox.
    pub(crate) fn acl_advertised(&self) -> bool {
        self.acl_advertised
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// Wraps a stream and consumes the server greeting.
    ///
    /// # Errors
    ///
    /// [`ImapError::Bye`] if the server greets with `* BYE` (refusing the
    /// connection), [`ImapError::Protocol`] on an unrecognized greeting, or
    /// [`ImapError::Io`] on a transport failure.
    pub(crate) async fn open(stream: S) -> ImapResult<Self> {
        let mut connection = Self {
            inner: BufReader::new(stream),
            tag: 0,
            qresync: false,
            idle_advertised: false,
            namespace_advertised: false,
            acl_advertised: false,
            pending_tag: None,
        };
        connection.read_greeting().await?;
        Ok(connection)
    }

    /// Whether QRESYNC (RFC 7162) is enabled for this session.
    pub(crate) fn qresync_enabled(&self) -> bool {
        self.qresync
    }

    /// Forces the QRESYNC flag on, for tests that drive the sync layer over a mock
    /// transcript without replaying the live `CAPABILITY`/`ENABLE` negotiation.
    #[cfg(test)]
    pub(crate) fn force_qresync(&mut self) {
        self.qresync = true;
    }

    /// Reads the untagged greeting: `* OK`/`* PREAUTH` is success, `* BYE` is a
    /// refusal.
    async fn read_greeting(&mut self) -> ImapResult<()> {
        let line = self.read_line().await?;
        let text = String::from_utf8_lossy(&line);
        if text.starts_with("* OK") || text.starts_with("* PREAUTH") {
            Ok(())
        } else if text.starts_with("* BYE") {
            Err(ImapError::bye(text.trim().to_owned()))
        } else {
            Err(ImapError::protocol(format!(
                "unexpected greeting: {}",
                text.trim()
            )))
        }
    }

    /// Allocates the next command tag (`a1`, `a2`, …). `pub(crate)` so the IDLE
    /// primitives in [`crate::idle`] can tag the `IDLE` command they manage outside
    /// the normal request/response [`command`](Self::command) round trip.
    pub(crate) fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("a{}", self.tag)
    }

    /// Writes raw bytes and flushes — the unframed send the IDLE primitives need to
    /// issue `<tag> IDLE\r\n` and the bare `DONE\r\n` continuation
    /// ([`crate::idle`]), which fall outside [`command`](Self::command)'s tagged
    /// request/response shape.
    pub(crate) async fn send_raw(&mut self, bytes: &[u8]) -> ImapResult<()> {
        self.inner.write_all(bytes).await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Reads one logical line: bytes through the next `\n`, with any `{n}` literal
    /// the line announces inlined (the n bytes, then the continuation). Literals
    /// can themselves announce further literals, so this loops.
    ///
    /// `pub(crate)` so the IDLE read loop ([`crate::idle`]) can consume the
    /// unsolicited untagged responses the server streams while idling, reusing the
    /// same literal-aware framing as the command path.
    pub(crate) async fn read_line(&mut self) -> ImapResult<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let before = line.len();
            let read = self.inner.read_until(b'\n', &mut line).await?;
            if read == 0 {
                return Err(ImapError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-response",
                )));
            }
            if let Some(len) = trailing_literal_len(&line[before..]) {
                if len > MAX_LITERAL {
                    return Err(ImapError::protocol(format!(
                        "server announced a {len}-byte literal exceeding the {MAX_LITERAL}-byte cap"
                    )));
                }
                let mut literal = vec![0u8; len];
                self.inner.read_exact(&mut literal).await?;
                line.extend_from_slice(&literal);
                continue;
            }
            return Ok(line);
        }
    }

    /// Sends a tagged command and collects its untagged responses and completion
    /// detail. A `NO`/`BAD` completion is an error. `pub(crate)` so the STARTTLS
    /// preamble (`crate::transport_starttls`) can issue `CAPABILITY`/`STARTTLS` over
    /// the plaintext connection reusing the tagged round trip.
    pub(crate) async fn command(&mut self, command: &str) -> ImapResult<Response> {
        // If a streamed `UID FETCH` was abandoned mid-response, finish reading it to
        // its tag first so this command's reply is not corrupted by leftover lines.
        self.drain_pending().await?;
        let tag = self.next_tag();
        let request = format!("{tag} {command}\r\n");
        self.inner.write_all(request.as_bytes()).await?;
        self.inner.flush().await?;
        self.read_response(&tag).await
    }

    /// Reads untagged responses until this command's tagged completion. `pub(crate)` so
    /// `APPEND` (`crate::transport_commands`), which writes its own tag ahead of a
    /// synchronizing literal, can finish the round trip through the same reader.
    pub(crate) async fn read_response(&mut self, tag: &str) -> ImapResult<Response> {
        let mut untagged = Vec::new();
        let prefix = format!("{tag} ");
        loop {
            let line = self.read_line().await?;
            if let Some(body) = strip_ascii_prefix(&line, b"* ") {
                untagged.push(body.to_vec());
                continue;
            }
            if strip_ascii_prefix(&line, b"+ ").is_some() {
                // We never send synchronizing literals in commands, so the server
                // should never ask for continuation.
                return Err(ImapError::protocol("unexpected continuation request"));
            }
            let text = String::from_utf8_lossy(&line);
            let Some(rest) = text.strip_prefix(&prefix) else {
                return Err(ImapError::protocol(format!(
                    "unexpected line: {}",
                    text.trim()
                )));
            };
            let mut parts = rest.trim_end().splitn(2, ' ');
            let status = parts.next().unwrap_or_default();
            let detail = parts.next().unwrap_or_default().to_owned();
            return match status.to_ascii_uppercase().as_str() {
                "OK" => Ok(Response { untagged, detail }),
                "NO" => Err(ImapError::no(detail)),
                "BAD" => Err(ImapError::bad(detail)),
                other => Err(ImapError::protocol(format!("unknown completion {other}"))),
            };
        }
    }

    /// `LOGIN user password`. A `NO` here is an authentication failure, not a
    /// generic invalid-state error.
    pub(crate) async fn login(&mut self, user: &str, password: &str) -> ImapResult<()> {
        let command = format!("LOGIN {} {}", quote(user), quote(password));
        match self.command(&command).await {
            Ok(_) => Ok(()),
            Err(ImapError::No(detail)) => Err(ImapError::auth(detail)),
            Err(other) => Err(other),
        }
    }

    /// Detects QRESYNC (RFC 7162) and, when the server advertises it, `ENABLE`s it so
    /// later deltas can use `CHANGEDSINCE`/`VANISHED` to reconcile flag changes and
    /// expunges incrementally. Capabilities are queried with an explicit `CAPABILITY`
    /// **after** login, because servers (Stalwart included) advertise CONDSTORE/QRESYNC
    /// only post-authentication. Best-effort: a server that lists QRESYNC but rejects
    /// `ENABLE` (a `NO`/`BAD`), or that answers `OK` without confirming `* ENABLED
    /// QRESYNC`, leaves the session in the non-QRESYNC baseline rather than failing the
    /// connection; a transport error still propagates.
    pub(crate) async fn negotiate_qresync(&mut self) -> ImapResult<()> {
        let response = self.command("CAPABILITY").await?;
        let capabilities = crate::parse_qresync::parse_capabilities(&response.into_all_lines());
        // Record IDLE (RFC 2177) from the same post-auth list so a watcher (and the
        // provider's advertised `Capabilities::idle`) knows whether push is available.
        let advertises = |name: &str| {
            capabilities
                .iter()
                .any(|cap| cap.eq_ignore_ascii_case(name))
        };
        self.idle_advertised = advertises("IDLE");
        // Shared-mailbox discovery needs both: `NAMESPACE` to know whose mail a folder
        // holds, `ACL` to ask what may be done in it. Recorded from the same post-auth
        // list, because — like CONDSTORE/QRESYNC — a server may advertise neither before
        // authentication.
        self.namespace_advertised = advertises("NAMESPACE");
        self.acl_advertised = advertises("ACL");
        if capabilities
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case("QRESYNC"))
        {
            match self.command("ENABLE QRESYNC").await {
                // Trust the enable only if `* ENABLED QRESYNC` confirms it (a bare
                // `* ENABLED` + OK enables nothing, RFC 5161); otherwise stay baseline.
                Ok(response) => {
                    if crate::parse_qresync::enabled_lists_qresync(&response.untagged) {
                        self.qresync = true;
                    }
                }
                Err(ImapError::No(_) | ImapError::Bad(_)) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }
}

/// Extracts `(validity, uid)` from an `[APPENDUID validity uid]` response code
/// (RFC 4315), if present.
pub(crate) fn parse_append_uid(detail: &str) -> Option<(u32, u32)> {
    let start = detail.find("[APPENDUID ")? + "[APPENDUID ".len();
    let rest = &detail[start..];
    let end = rest.find(']')?;
    let mut parts = rest[..end].split_whitespace();
    let validity = parts.next()?.parse().ok()?;
    let uid = parts.next()?.parse().ok()?;
    Some((validity, uid))
}

/// One command's untagged responses plus its completion detail. `pub(crate)` so
/// the STARTTLS preamble (`crate::transport_starttls`) can read a `CAPABILITY`
/// response through the shared [`Connection::command`].
pub(crate) struct Response {
    /// The untagged `* …` lines, with the `* ` peeled and any literals inlined.
    /// `pub(crate)` so the command vocabulary (`crate::transport_commands`) can hand them
    /// straight to a parser.
    pub(crate) untagged: Vec<Vec<u8>>,
    /// The tagged completion's text after the status word — where a response code like
    /// `[APPENDUID …]` may also appear.
    pub(crate) detail: String,
}

impl Response {
    /// The untagged lines plus the completion detail, consumed (no clone), so a
    /// `[UIDVALIDITY n]` response code in either place is seen.
    pub(crate) fn into_all_lines(self) -> Vec<Vec<u8>> {
        let mut lines = self.untagged;
        lines.push(self.detail.into_bytes());
        lines
    }
}

/// The literal length a line announces (`…{n}` or `…{n+}` before its CRLF), if any.
fn trailing_literal_len(line: &[u8]) -> Option<usize> {
    let trimmed = line.strip_suffix(b"\n")?;
    let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
    let inside = trimmed.strip_suffix(b"}")?;
    let inside = inside.strip_suffix(b"+").unwrap_or(inside);
    let open = inside.iter().rposition(|&b| b == b'{')?;
    let digits = &inside[open + 1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Strips an ASCII prefix, returning the remainder without its trailing CRLF.
/// `pub(crate)` so [`crate::idle`] can peel the `* ` from an untagged line before
/// classifying the IDLE notification it carries.
pub(crate) fn strip_ascii_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(prefix)?;
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    Some(rest.strip_suffix(b"\r").unwrap_or(rest))
}

/// Wraps a value as an IMAP quoted string, escaping `\` and `"`. `pub(crate)` so the
/// command vocabulary (`crate::transport_commands`) quotes its mailbox arguments with the
/// same escaping.
pub(crate) fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
