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

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    deadline::DATA_ACK_STALL,
    error::{ImapError, ImapResult},
    smtp_auth::{self, SmtpAuth},
    smtp_stream::SmtpStream,
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

/// Reads what a submission server advertises and nothing else: the greeting, `EHLO`,
/// then `QUIT`. No envelope, no credential, no message.
///
/// The [`send`] of the probe path ([`crate::probe`]), and paired with
/// [`extensions_after_starttls`] exactly as `send` is with [`send_after_starttls`],
/// because the greeting is read on one and already consumed on the other.
///
/// # Errors
///
/// [`ImapError::Protocol`] if the greeting is not a `220` or `EHLO`/`HELO` is refused.
pub(crate) async fn extensions<S>(stream: S, ehlo_domain: &str) -> ImapResult<Vec<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut smtp = SmtpStream::new(stream);
    read_greeting(&mut smtp).await?;
    finish_probe(&mut smtp, ehlo_domain).await
}

/// [`extensions`] over a stream already past its greeting and a `STARTTLS` upgrade.
///
/// This is the reading that counts for a STARTTLS server: `AUTH` is commonly withheld
/// until the link is secured, so the cleartext `EHLO` that negotiated the upgrade lists
/// mechanisms the account cannot actually use.
///
/// # Errors
///
/// [`ImapError::Protocol`] if `EHLO`/`HELO` is refused.
pub(crate) async fn extensions_after_starttls<S>(
    stream: S,
    ehlo_domain: &str,
) -> ImapResult<Vec<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut smtp = SmtpStream::new(stream);
    finish_probe(&mut smtp, ehlo_domain).await
}

/// `EHLO`, then `QUIT`: the shared tail of both probe entries.
///
/// The `QUIT` is not politeness. A submission server logs an abandoned socket as a
/// dropped connection, and an account whose setup is being diagnosed should not have a
/// row of those in its provider's log before it has connected once. Its reply is
/// discarded: the answer is already in hand, and a server that closes without one has
/// still answered.
async fn finish_probe<S>(smtp: &mut SmtpStream<S>, ehlo_domain: &str) -> ImapResult<Vec<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (_esmtp, extensions) = ehlo(smtp, ehlo_domain).await?;
    let _ = smtp.write_line("QUIT").await;
    Ok(extensions)
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
    // wire, so ANY failure to read the acknowledgement — a dropped connection, a reply
    // that never came, OR a malformed reply — is the ambiguous case: it may have
    // delivered, so it must be confirmed, never blind-retried (never a plain transport
    // error here).
    let disposition = match smtp.read_reply_lines_within(DATA_ACK_STALL).await {
        Ok((code, _)) if is_success(code) => Disposition::Delivered,
        Ok((code, lines)) => classify(code, lines.join(" ")),
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
    // The domain goes verbatim into the `EHLO`/`HELO` command line, so screen it here
    // rather than in each caller: `converse` guards the envelope addresses it owns, and
    // the probe entries ([`extensions`]/[`extensions_after_starttls`]) take the domain
    // straight from a host that has not yet authenticated anything. One CR would append
    // a second command to the line (RFC 5321 §2.3.8).
    let ehlo_domain = reject_control("EHLO domain", ehlo_domain)?;
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

#[cfg(test)]
#[path = "smtp_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "smtp_starttls_tests.rs"]
mod starttls_tests;

#[cfg(test)]
#[path = "smtp_probe_tests.rs"]
mod probe_tests;

#[cfg(test)]
#[path = "smtp_password_auth_tests.rs"]
mod password_auth_tests;
