//! IMAP transport: the tagged line protocol over any async stream.
//!
//! [`Connection`] is generic over the stream `S`, so the offline tests drive the
//! whole protocol over an in-memory mock while the live client uses a `tokio-rustls`
//! TLS stream — command sequencing, literal handling, and parsing are identical in
//! both (`docs/agent-guidance/imap-smtp.md`). It speaks only the handful of commands
//! the read/sync slice needs (`LOGIN`, `SELECT`, `UID SEARCH`, `UID FETCH`, `LIST`,
//! `LOGOUT`); the higher-level snapshot/delta logic lives in [`crate::sync`].

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::{ImapError, ImapResult};
use crate::parse::{self, FetchRow, ListRow, SelectData};

/// The largest `{n}` literal we will read into memory. A hostile or buggy server
/// could announce an enormous literal (`* {4000000000}`); the cap bounds the
/// allocation so adversarial input cannot exhaust memory (`north-star.md` security).
/// Generous enough for any real metadata response (and future body fetches).
const MAX_LITERAL: usize = 64 * 1024 * 1024;

/// A connected IMAP session over a generic async byte stream.
pub(crate) struct Connection<S> {
    inner: BufReader<S>,
    tag: u32,
    /// Whether QRESYNC (RFC 7162) was negotiated for this session — set by
    /// [`Connection::negotiate_qresync`]. When `true`, the sync layer opens mailboxes
    /// with CONDSTORE and reconciles deltas via `CHANGEDSINCE`/`VANISHED`.
    qresync: bool,
}

impl<S> core::fmt::Debug for Connection<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Connection")
            .field("tag", &self.tag)
            .field("qresync", &self.qresync)
            .finish_non_exhaustive()
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

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("a{}", self.tag)
    }

    /// Reads one logical line: bytes through the next `\n`, with any `{n}` literal
    /// the line announces inlined (the n bytes, then the continuation). Literals
    /// can themselves announce further literals, so this loops.
    async fn read_line(&mut self) -> ImapResult<Vec<u8>> {
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
    /// detail. A `NO`/`BAD` completion is an error.
    async fn command(&mut self, command: &str) -> ImapResult<Response> {
        let tag = self.next_tag();
        let request = format!("{tag} {command}\r\n");
        self.inner.write_all(request.as_bytes()).await?;
        self.inner.flush().await?;
        self.read_response(&tag).await
    }

    /// Reads untagged responses until this command's tagged completion.
    async fn read_response(&mut self, tag: &str) -> ImapResult<Response> {
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

    /// `SELECT mailbox`, returning its UID space and message count. Response codes
    /// in either an untagged `* OK [..]` or the tagged completion are honored.
    pub(crate) async fn select(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let response = self.command(&format!("SELECT {}", quote(mailbox))).await?;
        parse::parse_select(&response.into_all_lines())
    }

    /// `SELECT mailbox (CONDSTORE)` — opens the mailbox CONDSTORE-aware (RFC 7162
    /// §3.1.8) so the response carries `[HIGHESTMODSEQ n]`, the baseline a QRESYNC
    /// delta records in its cursor. Used in place of [`Connection::select`] for the
    /// sync path on a QRESYNC session.
    pub(crate) async fn select_condstore(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let response = self
            .command(&format!("SELECT {} (CONDSTORE)", quote(mailbox)))
            .await?;
        parse::parse_select(&response.into_all_lines())
    }

    /// `EXAMINE mailbox` — the read-only `SELECT` (RFC 9051 §6.3.2): same response
    /// shape, but opens the mailbox without write intent and does not reset
    /// `\Recent`, so a body peek needs no write access to the folder.
    pub(crate) async fn examine(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let response = self.command(&format!("EXAMINE {}", quote(mailbox))).await?;
        parse::parse_select(&response.into_all_lines())
    }

    /// `UID FETCH <set> (<items>)`, returning the parsed rows.
    pub(crate) async fn uid_fetch(&mut self, set: &str, items: &str) -> ImapResult<Vec<FetchRow>> {
        let response = self.command(&format!("UID FETCH {set} ({items})")).await?;
        parse::parse_fetch(&response.untagged)
    }

    /// `UID FETCH <set> (<items>) (CHANGEDSINCE <modseq> VANISHED)` — the QRESYNC
    /// incremental delta (RFC 7162 §3.1.4.1, §3.2.5). The server returns a `FETCH` for
    /// every message whose mod-sequence is greater than `modseq` (new arrivals *and*
    /// flag changes, with full metadata) and a `* VANISHED (EARLIER) <set>` listing the
    /// UIDs expunged since `modseq`. Returns the changed rows paired with the expanded
    /// vanished UIDs, both read from the one command's untagged responses.
    pub(crate) async fn uid_fetch_changedsince(
        &mut self,
        set: &str,
        items: &str,
        modseq: u64,
    ) -> ImapResult<(Vec<FetchRow>, Vec<u32>)> {
        let response = self
            .command(&format!(
                "UID FETCH {set} ({items}) (CHANGEDSINCE {modseq} VANISHED)"
            ))
            .await?;
        let rows = parse::parse_fetch(&response.untagged)?;
        let vanished = crate::parse_qresync::parse_vanished(&response.untagged);
        Ok((rows, vanished))
    }

    /// `UID SEARCH SINCE <date>` — the UIDs of messages whose `INTERNALDATE` is on or
    /// after `date` (an IMAP `dd-Mon-yyyy` date, RFC 9051 §6.4.4), used to find the
    /// floor of a sync-depth window so a snapshot fetches only recent mail. `date` is
    /// caller-formatted from a calendar date (digits + a fixed month abbreviation), so
    /// it carries no quoting or injection risk. Returns the matched UIDs (empty if none
    /// match), tolerating both the classic `* SEARCH` and extended `* ESEARCH` reply.
    pub(crate) async fn uid_search_since(&mut self, date: &str) -> ImapResult<Vec<u32>> {
        let response = self.command(&format!("UID SEARCH SINCE {date}")).await?;
        Ok(parse::parse_search(&response.untagged))
    }

    /// `UID FETCH <uid> (BODY.PEEK[])`, returning the raw RFC 5322 bytes of the
    /// message (the whole source, headers + every part), or `None` if the server
    /// returned no `BODY[]` for that UID — i.e. it was expunged since the last sync
    /// (fetching a non-existent UID is a tagged `OK` with no data, RFC 9051 §6.4.8).
    /// `.PEEK` does not set `\Seen` — fetching a body to read it must not silently
    /// mark it read; the host decides that via a separate edit. Only the matching
    /// UID's data is accepted, so an unsolicited `FETCH` for another UID (a
    /// concurrent flag update) cannot return the wrong message's bytes.
    pub(crate) async fn uid_fetch_body(&mut self, uid: u32) -> ImapResult<Option<Vec<u8>>> {
        let response = self
            .command(&format!("UID FETCH {uid} (BODY.PEEK[])"))
            .await?;
        Ok(parse::parse_fetch_body(&response.untagged, uid))
    }

    /// `LIST "" "*"`, returning every mailbox.
    pub(crate) async fn list(&mut self) -> ImapResult<Vec<ListRow>> {
        let response = self.command(r#"LIST "" "*""#).await?;
        parse::parse_list(&response.untagged)
    }

    /// `CREATE <mailbox>`. Used to ensure the Sent folder exists before filing a
    /// copy; an "already exists" rejection is the caller's to ignore.
    pub(crate) async fn create(&mut self, mailbox: &str) -> ImapResult<()> {
        self.command(&format!("CREATE {}", quote(mailbox))).await?;
        Ok(())
    }

    /// `APPEND <mailbox> (<flags>) {N}` followed by the message literal — used to
    /// file a sent copy in Sent (`\Seen`) or save a draft in Drafts (`\Draft`).
    /// Returns the `[APPENDUID validity uid]` when the server supports UIDPLUS, so
    /// the caller can key the object; `None` otherwise (it then reconciles by
    /// `Message-ID` on a later sync).
    pub(crate) async fn append(
        &mut self,
        mailbox: &str,
        flags: &str,
        message: &[u8],
    ) -> ImapResult<Option<(u32, u32)>> {
        let tag = self.next_tag();
        // A synchronizing literal: send the header, await the `+` continuation, then
        // the raw bytes.
        let header = format!(
            "{tag} APPEND {} ({flags}) {{{}}}\r\n",
            quote(mailbox),
            message.len()
        );
        self.inner.write_all(header.as_bytes()).await?;
        self.inner.flush().await?;
        // The server may emit untagged responses (e.g. `* n EXISTS`) before the `+`
        // continuation request; skip them and wait for the continuation (RFC 9051
        // §7 allows unsolicited untagged responses at any point).
        loop {
            let line = self.read_line().await?;
            if strip_ascii_prefix(&line, b"* ").is_some() {
                continue;
            }
            if strip_ascii_prefix(&line, b"+ ").is_some() {
                break;
            }
            return Err(ImapError::protocol(format!(
                "APPEND expected a continuation, got: {}",
                String::from_utf8_lossy(&line).trim()
            )));
        }
        self.inner.write_all(message).await?;
        self.inner.write_all(b"\r\n").await?;
        self.inner.flush().await?;
        let response = self.read_response(&tag).await?;
        Ok(parse_append_uid(&response.detail))
    }

    /// `UID STORE <set> <item>` — alters the flags of the named UIDs, where `item`
    /// is e.g. `+FLAGS.SILENT (\Seen)` or `-FLAGS.SILENT (\Flagged)` (RFC 9051
    /// §6.4.6). The `.SILENT` suffix suppresses the per-message `FETCH` echo, so no
    /// response parsing is needed — a tagged `OK` is success, a `NO`/`BAD` an error.
    pub(crate) async fn uid_store(&mut self, set: &str, item: &str) -> ImapResult<()> {
        self.command(&format!("UID STORE {set} {item}")).await?;
        Ok(())
    }

    /// `UID MOVE <set> <mailbox>` — moves the named UIDs to `dest` (RFC 6851), so
    /// the move is atomic server-side (copy + `\Deleted` + expunge in one command,
    /// where supported). The destination is a quoted string.
    pub(crate) async fn uid_move(&mut self, set: &str, dest: &str) -> ImapResult<()> {
        self.command(&format!("UID MOVE {set} {}", quote(dest)))
            .await?;
        Ok(())
    }

    /// `UID EXPUNGE <set>` — permanently removes only the named `\Deleted` UIDs
    /// (UIDPLUS, RFC 4315), so a concurrent `\Deleted` mark elsewhere in the mailbox
    /// is not collaterally expunged.
    pub(crate) async fn uid_expunge(&mut self, set: &str) -> ImapResult<()> {
        self.command(&format!("UID EXPUNGE {set}")).await?;
        Ok(())
    }
}

/// Extracts `(validity, uid)` from an `[APPENDUID validity uid]` response code
/// (RFC 4315), if present.
fn parse_append_uid(detail: &str) -> Option<(u32, u32)> {
    let start = detail.find("[APPENDUID ")? + "[APPENDUID ".len();
    let rest = &detail[start..];
    let end = rest.find(']')?;
    let mut parts = rest[..end].split_whitespace();
    let validity = parts.next()?.parse().ok()?;
    let uid = parts.next()?.parse().ok()?;
    Some((validity, uid))
}

/// One command's untagged responses plus its completion detail.
struct Response {
    untagged: Vec<Vec<u8>>,
    detail: String,
}

impl Response {
    /// The untagged lines plus the completion detail, consumed (no clone), so a
    /// `[UIDVALIDITY n]` response code in either place is seen.
    fn into_all_lines(self) -> Vec<Vec<u8>> {
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
fn strip_ascii_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(prefix)?;
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    Some(rest.strip_suffix(b"\r").unwrap_or(rest))
}

/// Wraps a value as an IMAP quoted string, escaping `\` and `"`.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
