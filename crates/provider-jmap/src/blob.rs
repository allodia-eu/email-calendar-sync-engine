//! Blob downloads: RFC 8620 §6.2 `downloadUrl` template substitution, and the raw
//! message-source fetch built on it.
//!
//! Split out of `fetch.rs` because two callers share the URL builder — this module's
//! `message_source` and the contact-photo download in `provider` — and because the
//! encoding rule below is a security invariant worth pinning with its own tests.

use std::fmt::Write as _;

use engine_core::{mail::Message, raw::RawMime};

use crate::{error::JmapError, provider::Executor};

/// Percent-encodes one `downloadUrl` placeholder substitution.
///
/// Everything outside RFC 3986's *unreserved* set is escaped, so a substituted value
/// can only ever be a single path/query token — it can never introduce a `?`, `#`,
/// `&`, or `/../` and thereby re-point or re-parameterize the URL. For a
/// spec-conforming JMAP id (RFC 8620 §1.2 restricts ids to `A-Za-z0-9_-`) this is a
/// no-op; it matters for the values that are **not** ids, notably a media type taken
/// from a server-supplied contact payload.
fn encode_placeholder(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Builds a blob download URL from the session's `downloadUrl` template,
/// percent-encoding every substitution (see [`encode_placeholder`]).
pub(crate) fn download_url(
    template: &str,
    account: &str,
    blob: &str,
    media_type: &str,
    name: &str,
) -> String {
    template
        .replace("{accountId}", &encode_placeholder(account))
        .replace("{blobId}", &encode_placeholder(blob))
        .replace("{type}", &encode_placeholder(media_type))
        .replace("{name}", &encode_placeholder(name))
}

/// Downloads a message's raw RFC 5322 source via the session's `downloadUrl`
/// blob template (RFC 8620 §6.2).
///
/// Substitutes the template's `{accountId}`/`{blobId}`/`{type}`/`{name}`
/// placeholders — `accountId` is the JMAP mail account, `blobId` is the message's
/// synced blob handle — and GETs the bytes. Every substitution is percent-encoded by
/// [`download_url`], so no placeholder value can alter the URL's
/// structure.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the message carries no `blobId` (it was never
/// synced with one), [`JmapError::Session`] if the server advertised no
/// `downloadUrl`, or a transport/HTTP error from the download.
/// Decodes a `data:` URI's base64 payload, or `None` if `uri` is not one.
///
/// A JSContact `media` entry may carry its image **inline** rather than by `blobId`:
/// a card that reached the server as a vCard with `PHOTO;ENCODING=b` has no blob to
/// reference, and the `uri` is the whole picture. Observed on Stalwart, and the same
/// shape the CardDAV adapter builds for the same card — so an adapter that only
/// understands `blobId` fails on a photo the protocol legitimately delivered.
pub(crate) fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let (header, payload) = uri.strip_prefix("data:")?.split_once(',')?;
    if !header.ends_with(";base64") {
        return None;
    }
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut bits = 0_u32;
    let mut count = 0_u8;
    for byte in payload.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let digit = alphabet.iter().position(|candidate| *candidate == byte)?;
        bits = (bits << 6) | u32::try_from(digit).ok()?;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(u8::try_from((bits >> count) & 0xFF).ok()?);
        }
    }
    Some(out)
}

pub(crate) async fn message_source(
    executor: &dyn Executor,
    message: &Message,
) -> Result<RawMime, JmapError> {
    let blob = message
        .blob_id
        .as_ref()
        .ok_or_else(|| JmapError::protocol("message has no blobId; cannot fetch source"))?;
    let account = executor.session().mail_account_id()?;
    let template = executor
        .session()
        .download_url()
        .ok_or_else(|| JmapError::session("server advertised no downloadUrl"))?;
    let url = download_url(
        template,
        account,
        blob.as_str(),
        "application/octet-stream",
        "message",
    );
    let bytes = executor.download(&url).await?;
    Ok(RawMime::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::download_url;

    const TEMPLATE: &str = "https://jmap.test/download/{accountId}/{blobId}/{name}?type={type}";

    #[test]
    fn a_conforming_substitution_is_unchanged() {
        // RFC 8620 §1.2 ids are already unreserved, so encoding must not alter them.
        assert_eq!(
            download_url(
                TEMPLATE,
                "acc-1",
                "blob_2",
                "application/octet-stream",
                "message"
            ),
            "https://jmap.test/download/acc-1/blob_2/message?type=application%2Foctet-stream"
        );
    }

    /// `media_type` comes from a server-supplied JSContact payload. Unencoded, a `?`,
    /// `#`, `&`, or `..` in it would re-point or re-parameterize the download URL.
    #[test]
    fn a_hostile_media_type_cannot_restructure_the_url() {
        let url = download_url(
            TEMPLATE,
            "acc-1",
            "blob-1",
            "../../evil?x=1#f",
            "contact-photo",
        );
        // The structural characters are all escaped …
        assert!(!url.contains("../"), "path traversal survived: {url}");
        assert!(!url.contains('#'), "fragment survived: {url}");
        // … and the query still has exactly the one `?` the template itself carries.
        assert_eq!(url.matches('?').count(), 1, "extra query delimiter: {url}");
        assert!(url.ends_with("type=..%2F..%2Fevil%3Fx%3D1%23f"), "{url}");
    }

    #[test]
    fn a_hostile_blob_id_cannot_escape_its_path_segment() {
        let url = download_url(
            TEMPLATE,
            "acc-1",
            "../../../etc/passwd",
            "text/plain",
            "message",
        );
        assert!(!url.contains("../"), "path traversal survived: {url}");
        assert!(url.contains("..%2F..%2F..%2Fetc%2Fpasswd"), "{url}");
    }

    #[test]
    fn non_ascii_is_encoded_as_utf8_bytes() {
        let url = download_url(TEMPLATE, "acc", "blob", "image/jpeg", "café");
        assert!(url.contains("caf%C3%A9"), "{url}");
    }
}
