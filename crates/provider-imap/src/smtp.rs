//! SMTP submission (RFC 5321): the conversation and RFC 5322 message assembly.
//!
//! Like the IMAP transport, the conversation is generic over the stream so it is
//! driven offline over a mock and live over a real socket. It captures the two
//! invariants `providers.md` calls out: **per-recipient acceptance/rejection**
//! before `DATA` (each `RCPT TO` reply), and the **post-`DATA` ambiguity** — when
//! the final acknowledgement is lost the send is [`Disposition::Ambiguous`], which
//! the caller turns into a `NeedsConfirmation` op rather than blind-retrying.
//!
//! Authentication is optional: against the fixture's plaintext MX (port 25) the
//! conversation is `EHLO → MAIL → RCPT* → DATA` with no auth; against a real
//! provider the caller supplies a TLS stream and credentials, and an `AUTH PLAIN`
//! step runs after `EHLO`. STARTTLS (port 587) is a later refinement — the TLS path
//! here is implicit TLS (the stream is already secured by the caller).

use engine_core::mail::EmailAddress;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use engine_provider::Draft;

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

/// Assembles the RFC 5322 message bytes for `draft`, stamped with `date` (CRLF
/// line endings).
///
/// The caller's pre-generated `Message-ID` is set verbatim so the sent copy
/// reconciles by it on a later sync (`store-and-sync.md`).
///
/// # Errors
///
/// Every header-interpolated value (`Message-ID`, addresses, subject, display
/// names, and the `In-Reply-To`/`References` threading ids) is rejected if it
/// carries a CR, LF, or NUL — RFC 5322 §2.2 forbids those in a header field body,
/// and allowing them would let a hostile draft inject extra headers or split the
/// message / SMTP command stream. A non-ASCII subject or display name is emitted as
/// an RFC 2047 `B` encoded-word, never raw 8-bit bytes, so the headers stay 7-bit
/// clean. A `Date` header is generated from `date` (RFC 5322 §3.6 requires it; for
/// an IMAP `APPEND` — `save_draft` or the Sent copy — no server is in the loop to
/// add one). For a reply or forward the `In-Reply-To` and `References` headers
/// (RFC 5322 §3.6.4) thread the message with its original; each is omitted when its
/// draft field is empty.
pub(crate) fn assemble_message(draft: &Draft, date: OffsetDateTime) -> ImapResult<Vec<u8>> {
    let message_id = reject_control("Message-ID", draft.message_id.as_str())?;
    let from = address_field(&draft.from)?;
    let to = draft
        .to
        .iter()
        .map(address_field)
        .collect::<ImapResult<Vec<_>>>()?
        .join(", ");
    let subject = encode_header_text(reject_control("subject", &draft.subject)?);
    let in_reply_to = match &draft.in_reply_to {
        Some(parent) => format!(
            "In-Reply-To: <{}>\r\n",
            reject_control("In-Reply-To", parent.as_str())?
        ),
        None => String::new(),
    };
    let references = if draft.references.is_empty() {
        String::new()
    } else {
        let ids = draft
            .references
            .iter()
            .map(|r| reject_control("References", r.as_str()).map(|id| format!("<{id}>")))
            .collect::<ImapResult<Vec<_>>>()?
            .join(" ");
        format!("References: {ids}\r\n")
    };
    let date = date
        .format(&Rfc2822)
        .map_err(|e| ImapError::protocol(format!("cannot format the Date header: {e}")))?;
    let headers = format!(
        "Date: {date}\r\nMessage-ID: <{message_id}>\r\nFrom: {from}\r\nTo: {to}\r\n\
         {in_reply_to}{references}Subject: {subject}\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
    );
    let mut message = headers.into_bytes();
    for line in normalize_body_lines(&draft.text_body) {
        message.extend_from_slice(line.as_bytes());
        message.extend_from_slice(b"\r\n");
    }
    Ok(message)
}

/// Rejects a header/command value carrying CR, LF, or NUL — the bytes that would
/// inject extra headers or split the SMTP command stream (RFC 5322 §2.2 / RFC 5321
/// §2.3.8). Returns the value unchanged when clean.
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

/// Formats one address as an RFC 5322 header value: `Display Name <email>` (the name
/// quoted when ASCII, RFC 2047-encoded when not), or bare `email`. The email is
/// rejected on CR/LF/NUL but never encoded — it goes verbatim into both the header
/// and the SMTP `MAIL`/`RCPT` command.
fn address_field(addr: &EmailAddress) -> ImapResult<String> {
    let email = reject_control("address", &addr.email)?;
    match &addr.name {
        Some(name) => {
            let name = encode_header_phrase(reject_control("display name", name)?);
            Ok(format!("{name} <{email}>"))
        }
        None => Ok(email.to_owned()),
    }
}

/// Whether `s` is entirely printable 7-bit ASCII (so it needs no encoding).
fn is_ascii_printable(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Encodes unstructured header text (a subject): verbatim when printable ASCII,
/// else an RFC 2047 `B` encoded-word.
fn encode_header_text(text: &str) -> String {
    if is_ascii_printable(text) {
        text.to_owned()
    } else {
        encoded_word(text)
    }
}

/// Encodes an address display-name phrase: a quoted-string when printable ASCII (so
/// specials like `,`/`.` are safe in the phrase position), else an RFC 2047 `B`
/// encoded-word.
fn encode_header_phrase(name: &str) -> String {
    if is_ascii_printable(name) {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        encoded_word(name)
    }
}

/// One RFC 2047 base64 encoded-word, `=?UTF-8?B?<base64>?=`. Long values are not yet
/// folded into 75-octet words (a later refinement); most subjects and names fit one.
fn encoded_word(text: &str) -> String {
    format!("=?UTF-8?B?{}?=", crate::base64::encode(text.as_bytes()))
}

/// Splits a body into lines on any of CRLF, a lone CR, or a lone LF, so a bare CR
/// from legacy text never reaches the wire (RFC 5321/5322 forbid a bare CR or LF).
/// Each returned line is re-emitted CRLF-terminated by the caller.
fn normalize_body_lines(body: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = body;
    loop {
        let Some(idx) = rest.find(['\r', '\n']) else {
            lines.push(rest);
            return lines;
        };
        lines.push(&rest[..idx]);
        // A `\r\n` is one break; a lone `\r` or `\n` is also one.
        let skip = if rest.as_bytes()[idx] == b'\r' && rest.as_bytes().get(idx + 1) == Some(&b'\n')
        {
            2
        } else {
            1
        };
        rest = &rest[idx + skip..];
    }
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

// The threading-header tests live in a sibling file: `smtp_tests.rs` is already at
// the 500-line limit, so the In-Reply-To/References cases go here rather than grow it.
#[cfg(test)]
#[path = "smtp_threading_tests.rs"]
mod threading_tests;
