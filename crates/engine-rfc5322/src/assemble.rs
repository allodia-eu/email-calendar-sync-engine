//! The RFC 5322 envelope + header assembly, wrapping the MIME body from [`mime`].

use engine_core::mail::EmailAddress;
use engine_provider::{Draft, ProviderError, ProviderResult};
use time::{OffsetDateTime, format_description::well_known::Rfc2822};

use crate::{base64, mime};

/// Whether the assembled message carries a `Bcc` header — the one difference between
/// the over-the-wire message and the filed Sent/Drafts copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BccHeader {
    /// Over-the-wire: omit `Bcc`. Bcc recipients are reached out-of-band (the SMTP
    /// envelope, or the server parsing the MIME), so no recipient can see the Bcc list.
    Omit,
    /// Filed Sent/Drafts copy: include `Bcc`, so the sender keeps a record of whom
    /// they Bcc'd while it stays private to them.
    Include,
}

/// Assembles the **over-the-wire** RFC 5322 message for `draft` — **without** a `Bcc`
/// header, so no recipient can see the Bcc list. Use [`assemble_filed_message`] for the
/// Sent/Drafts copy.
///
/// The caller's pre-generated `Message-ID` is set verbatim (so the sent copy reconciles
/// by it on a later sync); a non-ASCII subject or display name is emitted as an RFC 2047
/// `B` encoded-word; a `Date` header is generated from `date`; and a reply's
/// `In-Reply-To`/`References` (RFC 5322 §3.6.4) thread it with its original.
///
/// # Errors
///
/// A [`ProviderError::permanent`] if any header-interpolated value (`Message-ID`,
/// addresses, subject, display names, the threading ids, attachment media metadata)
/// carries CR/LF/NUL, or the `Date` cannot be formatted.
pub fn assemble_message(draft: &Draft, date: OffsetDateTime) -> ProviderResult<Vec<u8>> {
    assemble(draft, date, BccHeader::Omit)
}

/// Assembles the RFC 5322 message for the **filed Sent/Drafts copy** — identical to
/// [`assemble_message`] but **with** the `Bcc` header, so the sender's Sent folder
/// records whom they Bcc'd. This copy is filed locally (IMAP `APPEND`) or handed to a
/// server that strips the `Bcc` before delivery (Graph `sendMail`), so the Bcc never
/// reaches another recipient.
///
/// # Errors
///
/// A [`ProviderError::permanent`] on the same conditions as [`assemble_message`].
pub fn assemble_filed_message(draft: &Draft, date: OffsetDateTime) -> ProviderResult<Vec<u8>> {
    assemble(draft, date, BccHeader::Include)
}

/// Assembles the RFC 5322 message bytes for `draft`, stamped with `date` (CRLF line
/// endings), emitting a `Bcc` header only when `bcc` is [`BccHeader::Include`].
///
/// The caller's pre-generated `Message-ID` is set verbatim so the sent copy reconciles
/// by it on a later sync (`store-and-sync.md`).
///
/// # Errors
///
/// Every header-interpolated value (`Message-ID`, addresses, subject, display names,
/// the `In-Reply-To`/`References` threading ids, and attachment media metadata) is
/// rejected if it carries a CR, LF, or NUL — RFC 5322 §2.2 forbids those in a header
/// field body, and allowing them would let a hostile draft inject extra headers or
/// split the message / downstream command stream. A non-ASCII subject or display name
/// is emitted as an RFC 2047 `B` encoded-word, never raw 8-bit bytes, so the headers
/// stay 7-bit clean. A `Date` header is generated from `date` (RFC 5322 §3.6 requires
/// it; a locally filed copy has no server in the loop to add one). For a reply or
/// forward the `In-Reply-To` and `References` headers (RFC 5322 §3.6.4) thread the
/// message with its original; each is omitted when its draft field is empty. A `Cc`
/// header is emitted when the draft carries Cc recipients (visible to everyone); a
/// `Bcc` header is emitted only for [`BccHeader::Include`] (the filed copy), never for
/// transmission.
fn assemble(draft: &Draft, date: OffsetDateTime, bcc: BccHeader) -> ProviderResult<Vec<u8>> {
    let message_id = reject_control("Message-ID", draft.message_id.as_str())?;
    let from = address_field(&draft.from)?;
    // A message with no To recipients (a Bcc-only send) still needs a valid `To` header — name
    // an empty RFC 5322 §3.4 group, exactly as Outlook/Thunderbird do — rather than emit a bare
    // empty `To:` that many MTAs and spam filters penalize.
    let to_header = if draft.to.is_empty() {
        "To: undisclosed-recipients:;\r\n".to_owned()
    } else {
        format!("To: {}\r\n", address_list(&draft.to)?)
    };
    // A `Cc:` header is emitted (visible to every recipient) when present.
    let cc_header = if draft.cc.is_empty() {
        String::new()
    } else {
        format!("Cc: {}\r\n", address_list(&draft.cc)?)
    };
    // A `Bcc:` header is emitted ONLY for the filed Sent/Drafts copy (`BccHeader::Include`).
    // The transmitted message omits it, so Bcc recipients stay hidden from every recipient.
    let bcc_header = if bcc == BccHeader::Omit || draft.bcc.is_empty() {
        String::new()
    } else {
        format!("Bcc: {}\r\n", address_list(&draft.bcc)?)
    };
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
            .collect::<ProviderResult<Vec<_>>>()?
            .join(" ");
        format!("References: {ids}\r\n")
    };
    let date = date
        .format(&Rfc2822)
        .map_err(|e| ProviderError::permanent(format!("cannot format the Date header: {e}")))?;
    let headers = format!(
        "Date: {date}\r\nMessage-ID: <{message_id}>\r\nFrom: {from}\r\n{to_header}\
         {cc_header}{bcc_header}{in_reply_to}{references}Subject: {subject}\r\n\
         MIME-Version: 1.0\r\n",
    );
    let body = mime::assemble(draft)?;
    let mut message = headers.into_bytes();
    message.extend_from_slice(body.content_headers.as_bytes());
    message.extend_from_slice(b"\r\n");
    message.extend_from_slice(&body.body);
    Ok(message)
}

/// Rejects a header value carrying CR, LF, or NUL — the bytes that would inject extra
/// headers or split a downstream command stream (RFC 5322 §2.2 / RFC 5321 §2.3.8).
/// Returns the value unchanged when clean.
pub(crate) fn reject_control<'a>(field: &str, value: &'a str) -> ProviderResult<&'a str> {
    if value
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b'\0')
    {
        return Err(ProviderError::permanent(format!(
            "{field} contains a forbidden control character (CR, LF, or NUL)"
        )));
    }
    Ok(value)
}

/// Formats one address as an RFC 5322 header value: `Display Name <email>` (the name
/// quoted when ASCII, RFC 2047-encoded when not), or bare `email`. The email is
/// rejected on CR/LF/NUL but never encoded — it goes verbatim into the header (and,
/// for SMTP, the `MAIL`/`RCPT` command).
fn address_field(addr: &EmailAddress) -> ProviderResult<String> {
    let email = reject_control("address", &addr.email)?;
    match &addr.name {
        Some(name) => {
            let name = encode_header_phrase(reject_control("display name", name)?);
            Ok(format!("{name} <{email}>"))
        }
        None => Ok(email.to_owned()),
    }
}

/// Formats an address list as a comma-separated RFC 5322 header value (each via
/// [`address_field`]) — the shared body of the `To`/`Cc`/`Bcc` headers.
fn address_list(addresses: &[EmailAddress]) -> ProviderResult<String> {
    Ok(addresses
        .iter()
        .map(address_field)
        .collect::<ProviderResult<Vec<_>>>()?
        .join(", "))
}

/// Whether `s` is entirely printable 7-bit ASCII (so it needs no encoding).
pub(crate) fn is_ascii_printable(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Encodes unstructured header text (a subject): verbatim when printable ASCII, else an
/// RFC 2047 `B` encoded-word.
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
    format!("=?UTF-8?B?{}?=", base64::encode(text.as_bytes()))
}

/// Splits a body into lines on any of CRLF, a lone CR, or a lone LF, so a bare CR from
/// legacy text never reaches the wire (RFC 5321/5322 forbid a bare CR or LF). Each
/// returned line is re-emitted CRLF-terminated by the caller.
pub(crate) fn normalize_body_lines(body: &str) -> Vec<&str> {
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

#[cfg(test)]
#[path = "assemble_tests.rs"]
mod assemble_tests;

#[cfg(test)]
#[path = "threading_tests.rs"]
mod threading_tests;

#[cfg(test)]
#[path = "cc_bcc_tests.rs"]
mod cc_bcc_tests;

#[cfg(test)]
#[path = "mime_tests.rs"]
mod mime_tests;

#[cfg(test)]
#[path = "imip_tests.rs"]
mod imip_tests;
