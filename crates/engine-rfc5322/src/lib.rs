//! `engine-rfc5322` — outbound RFC 5322 / MIME message assembly.
//!
//! Turns an [`engine_provider::Draft`] into the message bytes a submission transport
//! sends. It is the one place the engine builds an RFC 5322 message, shared by every
//! adapter that submits raw MIME:
//!
//! - **`provider-imap`** feeds the bytes to an SMTP `DATA` command (RFC 5321).
//! - **`provider-graph`** base64-encodes them and `POST`s `/me/sendMail` in MIME format.
//!
//! Both preserve the caller's pre-generated `Message-ID` verbatim so the sent copy
//! reconciles by it on a later sync (the Write Contract, `store-and-sync.md`), and
//! both thread a reply through `In-Reply-To`/`References`. Keeping the assembler in
//! one crate keeps that RFC 5322 correctness — CRLF hardening, RFC 2047 encoded-words,
//! `multipart/{alternative,related,mixed}` nesting, exclusive-`DTEND`-style edge
//! cases — in one tested place rather than duplicated per transport.
//!
//! # Two assemblies, one difference
//!
//! [`assemble_message`] builds the **over-the-wire** copy — **without** a `Bcc`
//! header, so no recipient can see the Bcc list. [`assemble_filed_message`] builds
//! the **filed Sent/Drafts** copy — **with** the `Bcc` header, so the sender's own
//! folder records whom they Bcc'd (Outlook/Thunderbird behavior). They are identical
//! when the draft has no Bcc. A transport that hands the whole MIME to the server for
//! *both* delivery and filing (Graph `sendMail`) uses the filed variant, since the
//! server strips the `Bcc` header before delivery itself.
//!
//! # Hostile input
//!
//! Every header-interpolated value is screened for CR/LF/NUL (RFC 5322 §2.2), so a
//! hostile draft cannot inject extra headers or split the downstream command stream;
//! a rejected value is a [`ProviderError::permanent`](engine_provider::ProviderError::permanent)
//! (the message will never assemble unchanged). Non-ASCII subjects and display names
//! become RFC 2047 `B` encoded-words, never raw 8-bit header bytes.

mod assemble;
mod base64;
mod mime;

pub use assemble::{assemble_filed_message, assemble_message};

/// Standard RFC 4648 base64 of `bytes`.
///
/// The same codec the MIME attachment parts use, exposed because a transport that
/// hands the whole message to an HTTP API rather than an SMTP `DATA` command needs the
/// assembled bytes base64-encoded (Microsoft Graph `POST /me/sendMail` in MIME format
/// takes the message as a base64 `text/plain` body — `provider-graph`). Keeping the
/// codec here means the engine has exactly one base64 encoder for outbound mail.
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    base64::encode(bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_encode_exposes_the_standard_codec() {
        assert_eq!(super::base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
