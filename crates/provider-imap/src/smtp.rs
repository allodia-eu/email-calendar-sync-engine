//! SMTP submission (RFC 5321): the conversation.
//!
//! The RFC 5322 / MIME **message assembly** lives in `engine-rfc5322`
//! ([`engine_rfc5322::assemble_message`]/[`assemble_filed_message`]), shared with the
//! Graph adapter's MIME send; this module is the SMTP-specific half — the wire
//! conversation. Like the IMAP transport, it is generic over the stream so it is
//! driven offline over a mock and live over a real socket. It captures the two
//! invariants `providers.md` calls out: **per-recipient acceptance/rejection**
//! before `DATA` (each `RCPT TO` reply), and the **post-`DATA` ambiguity** — when
//! the final acknowledgement is lost the send is [`Disposition::Ambiguous`], which
//! the caller turns into a `NeedsConfirmation` op rather than blind-retrying.
//!
//! [`assemble_filed_message`]: engine_rfc5322::assemble_filed_message
//!
//! Three transports, all through this one conversation core ([`converse`]):
//! - **plaintext** ([`send`], no auth) — the fixture's local MX (port 25);
//! - **implicit TLS** ([`send`] with `auth`) — the caller hands an already-secured stream (port
//!   465), and `AUTH` runs after `EHLO`;
//! - **STARTTLS** ([`negotiate_starttls`] then [`send_after_starttls`]) — this module negotiates
//!   the cleartext upgrade (port 587) and the caller TLS-wraps the socket between the two calls;
//!   `AUTH` then runs over the established TLS.
//!
//! Authentication ([`crate::smtp_auth`]) is only ever sent once the stream is secured
//! (implicit TLS, or after the STARTTLS upgrade) — never in the clear. Which mechanism
//! runs there follows from the credential: `AUTH PLAIN` for a password, `AUTH
//! OAUTHBEARER`/`AUTH XOAUTH2` for an OAuth 2.0 access token.

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::{
    error::{ImapError, ImapResult},
    smtp_auth::{self, SmtpAuth},
};

/// One recipient's disposition from its `RCPT TO` reply (before `DATA`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recipient {
    /// The recipient address.
    pub address: String,
    /// Whether the server accepted it (a 2xx reply).
    pub accepted: bool,
    /// The server's reply text.
    pub response: String,
}

/// The final disposition of a submission after `DATA`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// The message was accepted (post-`DATA` 2xx).
    Delivered,
    /// Permanently rejected (a 5xx); do not retry.
    RejectedPermanent(String),
    /// Transiently declined (a 4xx); retry later. The message was *not* queued.
    RejectedTransient(String),
    /// The post-`DATA` acknowledgement was lost: it may or may not have delivered,
    /// so it must be confirmed, never blind-retried.
    Ambiguous(String),
}

/// The outcome of an SMTP submission: per-recipient results plus the final
/// disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmtpResult {
    /// Each recipient's accept/reject.
    pub recipients: Vec<Recipient>,
    /// What happened to the message itself.
    pub disposition: Disposition,
}

/// Runs the SMTP conversation over a **fresh** `stream`: reads the greeting, then
/// `EHLO → [AUTH] → MAIL → RCPT* → DATA`, identifying as `ehlo_domain`. When `auth` is
/// `Some`, authenticates after `EHLO` (only meaningful over TLS — the caller supplies an
/// already-secured stream). The plaintext / implicit-TLS entry.
pub(crate) async fn send<S>(
    stream: S,
    ehlo_domain: &str,
    from: &str,
    to: &[String],
    message: &[u8],
    auth: Option<SmtpAuth<'_>>,
) -> ImapResult<SmtpResult>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut smtp = SmtpStream::new(stream);
    read_greeting(&mut smtp).await?;
    converse(&mut smtp, ehlo_domain, from, to, message, auth).await
}

/// Runs the conversation over a stream already **past** its greeting and a `STARTTLS`
/// upgrade: the client sends `EHLO` directly (a server sends no fresh greeting after
/// `STARTTLS`), so this is [`converse`] with `AUTH` over the now-established TLS.
/// Paired with [`negotiate_starttls`], which the caller runs (and TLS-wraps the socket)
/// before this.
pub(crate) async fn send_after_starttls<S>(
    stream: S,
    ehlo_domain: &str,
    from: &str,
    to: &[String],
    message: &[u8],
    auth: Option<SmtpAuth<'_>>,
) -> ImapResult<SmtpResult>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut smtp = SmtpStream::new(stream);
    converse(&mut smtp, ehlo_domain, from, to, message, auth).await
}

/// The plaintext half of a `STARTTLS` submission (RFC 3207): reads the greeting,
/// `EHLO`s, confirms the server advertises `STARTTLS` (refusing otherwise — so
/// credentials never cross an un-upgradable link), issues `STARTTLS`, and on the `220`
/// returns the underlying stream for the caller to TLS-wrap. The conversation then
/// continues over TLS via [`send_after_starttls`].
///
/// # Errors
///
/// [`ImapError::Protocol`] if `STARTTLS` is not advertised or the `220` does not
/// arrive, or if any bytes are buffered past the `220` — a conformant server sends
/// nothing between it and the client-initiated TLS handshake, so buffered plaintext is
/// a command-injection attempt (CVE-2011-0411 class) that must not cross the boundary.
pub(crate) async fn negotiate_starttls<S>(stream: S, ehlo_domain: &str) -> ImapResult<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut smtp = SmtpStream::new(stream);
    read_greeting(&mut smtp).await?;
    let (_esmtp, extensions) = ehlo(&mut smtp, ehlo_domain).await?;
    if !extensions.iter().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("STARTTLS"))
    }) {
        return Err(ImapError::protocol(
            "server does not advertise STARTTLS; refusing to authenticate in the clear",
        ));
    }
    smtp.write_line("STARTTLS").await?;
    let (code, text) = smtp.read_reply().await?;
    if code != 220 {
        return Err(ImapError::protocol(format!(
            "STARTTLS refused: {code} {text}"
        )));
    }
    smtp.into_inner_stream()
}

/// Rejects an SMTP command value carrying CR, LF, or NUL — the bytes that would
/// inject an extra command or split the command stream (RFC 5321 §2.3.8). Returns the
/// value unchanged when clean. (Header-value screening during message assembly lives
/// in `engine-rfc5322`.)
fn reject_control<'a>(field: &str, value: &'a str) -> ImapResult<&'a str> {
    if value
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b'\0')
    {
        return Err(ImapError::protocol(format!(
            "{field} contains a forbidden control character (CR, LF, or NUL)"
        )));
    }
    Ok(value)
}

/// The conversation core shared by every transport: validates the envelope addresses,
/// `EHLO`s, optionally authenticates, then `MAIL → RCPT* → DATA` and classifies the
/// outcome. Assumes the greeting has already been read (both entries do so, or — for
/// STARTTLS — the upgrade consumed it).
async fn converse<S>(
    smtp: &mut SmtpStream<S>,
    ehlo_domain: &str,
    from: &str,
    to: &[String],
    message: &[u8],
    auth: Option<SmtpAuth<'_>>,
) -> ImapResult<SmtpResult>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // The envelope addresses go verbatim into `MAIL FROM`/`RCPT TO` command lines,
    // so reject any CR/LF/NUL before they can inject a command (RFC 5321 §2.3.8).
    reject_control("MAIL FROM address", from)?;
    for address in to {
        reject_control("RCPT TO address", address)?;
    }

    let (esmtp, extensions) = ehlo(smtp, ehlo_domain).await?;

    if let Some(auth) = auth {
        if !esmtp {
            return Err(ImapError::protocol("SMTP AUTH requires ESMTP (EHLO)"));
        }
        smtp_auth::authenticate(smtp, &auth, &extensions).await?;
    }

    smtp.write_line(&format!("MAIL FROM:<{from}>")).await?;
    let (code, text) = smtp.read_reply().await?;
    if !is_success(code) {
        return Ok(SmtpResult {
            recipients: Vec::new(),
            disposition: classify(code, text),
        });
    }

    let mut recipients = Vec::with_capacity(to.len());
    for address in to {
        smtp.write_line(&format!("RCPT TO:<{address}>")).await?;
        let (code, text) = smtp.read_reply().await?;
        recipients.push(Recipient {
            address: address.clone(),
            accepted: is_success(code),
            response: text,
        });
    }
    if !recipients.iter().any(|r| r.accepted) {
        let _ = smtp.write_line("QUIT").await;
        return Ok(SmtpResult {
            recipients,
            disposition: Disposition::RejectedPermanent("all recipients rejected".to_owned()),
        });
    }

    smtp.write_line("DATA").await?;
    let (code, text) = smtp.read_reply().await?;
    if code != 354 {
        return Ok(SmtpResult {
            recipients,
            disposition: classify(code, text),
        });
    }
    smtp.write_data(message).await?;

    // The post-DATA reply decides delivery. The message bytes are already on the
    // wire, so ANY failure to read the acknowledgement — a dropped connection OR a
    // malformed reply — is the ambiguous case: it may have delivered, so it must be
    // confirmed, never blind-retried (never a plain transport error here).
    let disposition = match smtp.read_reply().await {
        Ok((code, _)) if is_success(code) => Disposition::Delivered,
        Ok((code, text)) => classify(code, text),
        Err(_) => Disposition::Ambiguous("post-DATA acknowledgement unreadable".to_owned()),
    };
    let _ = smtp.write_line("QUIT").await;
    Ok(SmtpResult {
        recipients,
        disposition,
    })
}

/// Reads and checks the `220` greeting.
async fn read_greeting<S>(smtp: &mut SmtpStream<S>) -> ImapResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (code, _) = smtp.read_reply().await?;
    if code != 220 {
        return Err(ImapError::protocol(format!(
            "unexpected SMTP greeting code {code}"
        )));
    }
    Ok(())
}

/// Sends `EHLO`, falling back to `HELO` for a non-ESMTP server. Returns whether ESMTP
/// was accepted and the reply's lines — one advertised extension each (RFC 5321
/// §4.1.1.1), the first being the greeting text.
///
/// Line by line rather than joined, because an extension keyword only means anything at
/// the **start** of its own line. Reading `AUTH` or `STARTTLS` out of a flattened reply
/// lets a server's greeting prose ("… ready, no STARTTLS here") answer a question it was
/// never asked — the same trap `Response::untagged` exists for on the IMAP side.
async fn ehlo<S>(smtp: &mut SmtpStream<S>, ehlo_domain: &str) -> ImapResult<(bool, Vec<String>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    smtp.write_line(&format!("EHLO {ehlo_domain}")).await?;
    let (code, lines) = smtp.read_reply_lines().await?;
    if code == 250 {
        return Ok((true, lines));
    }
    // Fall back to HELO for a server without ESMTP.
    smtp.write_line(&format!("HELO {ehlo_domain}")).await?;
    let (code, lines) = smtp.read_reply_lines().await?;
    if code != 250 {
        return Err(ImapError::protocol(format!("EHLO/HELO refused: {code}")));
    }
    Ok((false, lines))
}

fn is_success(code: u16) -> bool {
    (200..300).contains(&code)
}

/// Classifies a non-success reply: 4xx is transient (retryable; not queued), any
/// other non-2xx is permanent.
fn classify(code: u16, text: String) -> Disposition {
    if (400..500).contains(&code) {
        Disposition::RejectedTransient(text)
    } else {
        Disposition::RejectedPermanent(text)
    }
}

/// A line-based SMTP stream with multiline-reply assembly. `pub(crate)` so
/// [`crate::smtp_auth`] can drive the `AUTH` exchange over the same stream (it is split
/// out only to keep this file under the size limit).
pub(crate) struct SmtpStream<S> {
    inner: BufReader<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> SmtpStream<S> {
    fn new(stream: S) -> Self {
        Self {
            inner: BufReader::new(stream),
        }
    }

    /// Unwraps the underlying stream after `STARTTLS`, for the TLS upgrade.
    ///
    /// Errors if the read buffer holds any bytes past the `STARTTLS` `220`: a
    /// conformant server sends nothing before the client-initiated TLS handshake, so
    /// buffered plaintext is a command-injection attempt (CVE-2011-0411 class) and
    /// MUST NOT be carried across the TLS boundary.
    fn into_inner_stream(self) -> ImapResult<S> {
        if !self.inner.buffer().is_empty() {
            return Err(ImapError::protocol(
                "unexpected buffered data after STARTTLS (possible command injection)",
            ));
        }
        Ok(self.inner.into_inner())
    }

    /// Reads a (possibly multiline) reply, returning its code and joined text — for
    /// every reply whose content is prose (a greeting, an acceptance, a rejection).
    /// [`read_reply_lines`](Self::read_reply_lines) is the form to use when the content
    /// is a *list* (`EHLO`'s extensions).
    pub(crate) async fn read_reply(&mut self) -> ImapResult<(u16, String)> {
        let (code, lines) = self.read_reply_lines().await?;
        Ok((code, lines.join(" ")))
    }

    /// Reads a (possibly multiline) reply, returning its code and one string per line
    /// (each stripped of its `NNN`/`NNN-` prefix). The continuation-line count is capped
    /// so a server emitting an endless stream of `NNN-...` lines cannot hang the
    /// submission or grow the reply without bound.
    pub(crate) async fn read_reply_lines(&mut self) -> ImapResult<(u16, Vec<String>)> {
        const MAX_REPLY_LINES: usize = 256;
        let mut lines = Vec::new();
        for _ in 0..MAX_REPLY_LINES {
            let mut line = String::new();
            if self.inner.read_line(&mut line).await? == 0 {
                return Err(ImapError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "SMTP connection closed",
                )));
            }
            let trimmed = line.trim_end();
            let code: u16 = trimmed
                .get(0..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| ImapError::protocol(format!("malformed SMTP reply: {trimmed}")))?;
            lines.push(trimmed.get(4..).unwrap_or("").to_owned());
            if trimmed.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, lines));
            }
        }
        Err(ImapError::protocol(
            "SMTP multiline reply exceeded the line cap",
        ))
    }

    pub(crate) async fn write_line(&mut self, line: &str) -> ImapResult<()> {
        self.inner.write_all(line.as_bytes()).await?;
        self.inner.write_all(b"\r\n").await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Writes the message body dot-stuffed, then the `<CRLF>.<CRLF>` terminator.
    async fn write_data(&mut self, message: &[u8]) -> ImapResult<()> {
        self.inner.write_all(&dot_stuff(message)).await?;
        self.inner.write_all(b".\r\n").await?;
        self.inner.flush().await?;
        Ok(())
    }
}

/// Dot-stuffs a CRLF-delimited message: any line beginning with `.` gets a second
/// leading `.` so it is not mistaken for the terminator (RFC 5321 §4.5.2).
fn dot_stuff(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    let mut start = 0;
    while start < message.len() {
        let end = message[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(message.len(), |p| start + p + 1);
        let line = &message[start..end];
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        start = end;
    }
    out
}

#[cfg(test)]
#[path = "smtp_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "smtp_starttls_tests.rs"]
mod starttls_tests;
