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
//! Authentication is optional: against the fixture's plaintext MX (port 25) the
//! conversation is `EHLO → MAIL → RCPT* → DATA` with no auth; against a real
//! provider the caller supplies a TLS stream and credentials, and an `AUTH PLAIN`
//! step runs after `EHLO`. STARTTLS (port 587) is a later refinement — the TLS path
//! here is implicit TLS (the stream is already secured by the caller).

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::{ImapError, ImapResult};

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

/// Runs the SMTP conversation over `stream`, submitting `message` from `from` to
/// `to`, identifying as `ehlo_domain`. When `auth` is `Some`, authenticates with
/// `AUTH PLAIN` after `EHLO` (only meaningful over TLS — the caller supplies a TLS
/// stream).
pub(crate) async fn send<S>(
    stream: S,
    ehlo_domain: &str,
    from: &str,
    to: &[String],
    message: &[u8],
    auth: Option<(&str, &str)>,
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

    let mut smtp = SmtpStream::new(stream);

    let (code, _) = smtp.read_reply().await?;
    if code != 220 {
        return Err(ImapError::protocol(format!(
            "unexpected SMTP greeting code {code}"
        )));
    }

    smtp.write_line(&format!("EHLO {ehlo_domain}")).await?;
    let (code, _) = smtp.read_reply().await?;
    let esmtp = code == 250;
    if !esmtp {
        // Fall back to HELO for a server without ESMTP.
        smtp.write_line(&format!("HELO {ehlo_domain}")).await?;
        let (code, _) = smtp.read_reply().await?;
        if code != 250 {
            return Err(ImapError::protocol(format!("EHLO/HELO refused: {code}")));
        }
    }

    if let Some((user, pass)) = auth {
        if !esmtp {
            return Err(ImapError::protocol("SMTP AUTH requires ESMTP (EHLO)"));
        }
        smtp.write_line(&format!("AUTH PLAIN {}", auth_plain_token(user, pass)))
            .await?;
        let (code, text) = smtp.read_reply().await?;
        if code != 235 {
            return Err(ImapError::auth(format!(
                "SMTP AUTH rejected: {code} {text}"
            )));
        }
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

/// A line-based SMTP stream with multiline-reply assembly.
struct SmtpStream<S> {
    inner: BufReader<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> SmtpStream<S> {
    fn new(stream: S) -> Self {
        Self {
            inner: BufReader::new(stream),
        }
    }

    /// Reads a (possibly multiline) reply, returning its code and joined text. The
    /// continuation-line count is capped so a server emitting an endless stream of
    /// `NNN-...` lines cannot hang the submission or grow `text` without bound.
    async fn read_reply(&mut self) -> ImapResult<(u16, String)> {
        const MAX_REPLY_LINES: usize = 256;
        let mut text = String::new();
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
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(trimmed.get(4..).unwrap_or(""));
            if trimmed.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text));
            }
        }
        Err(ImapError::protocol(
            "SMTP multiline reply exceeded the line cap",
        ))
    }

    async fn write_line(&mut self, line: &str) -> ImapResult<()> {
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

/// The `AUTH PLAIN` SASL token: base64 of `\0user\0password` (RFC 4616).
fn auth_plain_token(user: &str, password: &str) -> String {
    let mut creds = vec![0u8];
    creds.extend_from_slice(user.as_bytes());
    creds.push(0);
    creds.extend_from_slice(password.as_bytes());
    crate::base64::encode(&creds)
}

#[cfg(test)]
#[path = "smtp_tests.rs"]
mod tests;
