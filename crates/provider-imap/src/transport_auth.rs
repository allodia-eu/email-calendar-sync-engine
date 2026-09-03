//! The IMAP `AUTHENTICATE` half of the transport: SASL OAuth 2.0.
//!
//! Split from `transport` (which is at the file-size limit) but operating on the same
//! [`Connection`], exactly as the STARTTLS and `APPEND` halves are. `LOGIN` stays in
//! `transport` because it is a one-line command with a one-line answer; SASL is a
//! *conversation* — an optional initial response (RFC 4959), an error challenge the
//! client must acknowledge before the server will report the failure, and a mechanism
//! chosen from what the server advertises — and that does not fit alongside it.
//!
//! The mechanism-specific bytes live in [`crate::sasl`]; this module is the framing.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{ImapError, ImapResult},
    sasl::{self, Mechanism},
    transport::{Connection, strip_ascii_prefix},
};

/// How many server continuations one `AUTHENTICATE` may produce before the exchange is
/// abandoned. Both mechanisms here take at most two (the SASL-IR-less prompt and one
/// error challenge); the cap is what stops a server that answers every acknowledgement
/// with another challenge from parking the dial forever.
const MAX_CONTINUATIONS: usize = 8;

/// One line read during an `AUTHENTICATE` exchange, once the untagged noise a server
/// may interleave has been skipped.
enum SaslLine {
    /// `+ <base64>` — the server wants something. Either its request for the initial
    /// response (when SASL-IR was not used) or, after the credential, the error
    /// challenge describing a rejection.
    Continuation(String),
    /// The tagged completion: its status word and the rest of the line.
    Completion(String, String),
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// Issues `CAPABILITY` and returns the advertised atoms.
    ///
    /// Called at three different points, which is why it is one function: before
    /// `STARTTLS` (is the upgrade offered?), before `AUTHENTICATE` (which SASL
    /// mechanisms, and is SASL-IR usable?), and after authenticating (what does this
    /// session get — a server may advertise a quite different set once it knows who is
    /// asking, which is why the post-auth call is not an optimization away).
    pub(crate) async fn capabilities(&mut self) -> ImapResult<Vec<String>> {
        let response = self.command("CAPABILITY").await?;
        Ok(crate::parse_qresync::parse_capabilities(
            &response.into_all_lines(),
        ))
    }

    /// Authenticates with an OAuth 2.0 access token over SASL, choosing the mechanism
    /// from the server's own `AUTH=` capabilities ([`sasl::select`]).
    ///
    /// `host`/`port` describe the server being dialed; they ride the `OAUTHBEARER`
    /// response and are ignored by `XOAUTH2`.
    ///
    /// # Errors
    ///
    /// [`ImapError::Auth`] when the server advertises no OAuth mechanism (the message
    /// names what it *did* offer, because the usual cause is a token handed to an
    /// account that only takes a password) or when it rejects the token — carrying the
    /// decoded challenge, which is the only place the server says whether the token was
    /// expired, wrongly scoped, or for another account.
    pub(crate) async fn authenticate_oauth2(
        &mut self,
        username: &str,
        access_token: &str,
        host: &str,
        port: Option<u16>,
    ) -> ImapResult<()> {
        let capabilities = self.capabilities().await?;
        let offered: Vec<&str> = capabilities
            .iter()
            .filter_map(|atom| sasl::advertised_mechanism(atom))
            .collect();
        let mechanism = sasl::select(offered.iter().copied()).ok_or_else(|| {
            ImapError::auth(format!(
                "server advertises no OAuth SASL mechanism (it offers: {})",
                if offered.is_empty() {
                    "none".to_owned()
                } else {
                    offered.join(" ")
                }
            ))
        })?;
        let initial = mechanism.initial_response(username, access_token, host, port)?;
        // RFC 4959's initial response saves a round trip, but only where the server
        // says it takes one: an unadvertised initial response is a syntax error, and
        // the two-step exchange below works everywhere.
        let sasl_ir = capabilities
            .iter()
            .any(|atom| atom.eq_ignore_ascii_case("SASL-IR"));
        self.run_sasl(mechanism, &initial, sasl_ir).await
    }

    /// Drives one `AUTHENTICATE` exchange to its tagged completion.
    async fn run_sasl(
        &mut self,
        mechanism: Mechanism,
        initial: &str,
        sasl_ir: bool,
    ) -> ImapResult<()> {
        // A streamed `UID FETCH` abandoned mid-response would otherwise have its
        // leftover lines read as this exchange's, exactly as `command` guards against.
        self.drain_pending().await?;
        let tag = self.next_tag();
        let atom = mechanism.atom();
        if sasl_ir {
            self.send_raw(format!("{tag} AUTHENTICATE {atom} {initial}\r\n").as_bytes())
                .await?;
        } else {
            self.send_raw(format!("{tag} AUTHENTICATE {atom}\r\n").as_bytes())
                .await?;
            match self.next_sasl_line(&tag).await? {
                // The empty continuation is the server asking for the credential it
                // would have taken inline.
                SaslLine::Continuation(_) => {
                    self.send_raw(format!("{initial}\r\n").as_bytes()).await?;
                }
                // A server may refuse the mechanism outright rather than prompt.
                SaslLine::Completion(status, detail) => return complete(&status, &detail, ""),
            }
        }
        let mut challenge = String::new();
        for _ in 0..MAX_CONTINUATIONS {
            match self.next_sasl_line(&tag).await? {
                SaslLine::Continuation(payload) => {
                    // The rejection, described. Both mechanisms then wait for an
                    // acknowledgement before reporting the failure through the
                    // protocol's own error path — sending none leaves the connection
                    // parked mid-SASL, turning a stale token into a hang.
                    challenge = sasl::describe_challenge(&payload);
                    self.send_raw(format!("{}\r\n", mechanism.cancel_response()).as_bytes())
                        .await?;
                }
                SaslLine::Completion(status, detail) => {
                    return complete(&status, &detail, &challenge);
                }
            }
        }
        Err(ImapError::protocol(
            "server kept issuing SASL challenges without completing AUTHENTICATE",
        ))
    }

    /// Reads to the next line that decides something, skipping the untagged responses
    /// a server may interleave at any point (RFC 9051 §7).
    async fn next_sasl_line(&mut self, tag: &str) -> ImapResult<SaslLine> {
        let prefix = format!("{tag} ");
        loop {
            let line = self.read_line().await?;
            if strip_ascii_prefix(&line, b"* ").is_some() {
                continue;
            }
            if let Some(payload) = continuation_payload(&line) {
                return Ok(SaslLine::Continuation(
                    String::from_utf8_lossy(payload).into_owned(),
                ));
            }
            let text = String::from_utf8_lossy(&line);
            let Some(rest) = text.strip_prefix(&prefix) else {
                return Err(ImapError::protocol(format!(
                    "unexpected line during AUTHENTICATE: {}",
                    text.trim()
                )));
            };
            let mut parts = rest.trim_end().splitn(2, ' ');
            let status = parts.next().unwrap_or_default().to_owned();
            let detail = parts.next().unwrap_or_default().to_owned();
            return Ok(SaslLine::Completion(status, detail));
        }
    }
}

/// Classifies the tagged completion of an `AUTHENTICATE`, folding in the decoded
/// challenge when the server sent one.
///
/// A `NO` is an authentication failure (as it is for `LOGIN`), so a host is told to
/// refresh rather than to retry. A `BAD` is not: it means the command itself was
/// refused, which no new token fixes.
fn complete(status: &str, detail: &str, challenge: &str) -> ImapResult<()> {
    let described = if challenge.is_empty() {
        detail.to_owned()
    } else {
        format!("{detail} ({challenge})")
    };
    match status.to_ascii_uppercase().as_str() {
        "OK" => Ok(()),
        "NO" => Err(ImapError::auth(described)),
        "BAD" => Err(ImapError::bad(described)),
        other => Err(ImapError::protocol(format!(
            "unknown AUTHENTICATE completion {other}"
        ))),
    }
}

/// The payload of a `+` continuation line, or `None` if the line is not one.
///
/// Both spellings occur: a bare `+` (Stalwart's prompt) and `+ <base64>` (the error
/// challenge). Reading only the second would leave the SASL-IR-less exchange waiting
/// for a line the server already sent.
fn continuation_payload(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"+")?;
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    let rest = rest.strip_suffix(b"\r").unwrap_or(rest);
    Some(rest.strip_prefix(b" ").unwrap_or(rest))
}

#[cfg(test)]
#[path = "transport_auth_tests.rs"]
mod tests;
