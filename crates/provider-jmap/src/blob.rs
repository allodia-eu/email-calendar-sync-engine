//! Blob URLs: the session's `downloadUrl` and `uploadUrl` templates filled in (RFC 8620 §6),
//! and the raw message-source fetch built on them.
//!
//! The URL builders are public because a host that drives
//! [`JmapClient::download`](crate::JmapClient::download) or
//! [`JmapClient::upload`](crate::JmapClient::upload) itself needs the same URL the adapter
//! would build, and the encoding rule below is a security invariant that belongs in one place:
//! every substituted value is percent-encoded, so no value can re-point the URL.

use std::fmt::Write as _;

use engine_core::{ids::BlobId, mail::Message, raw::RawMime};

use crate::{error::JmapError, executor::Executor, session_accounts::JmapAccountId};

/// Percent-encodes one placeholder substitution.
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

/// The URL that downloads `blob` from `account`: the session's `downloadUrl` `template`
/// ([`Session::download_url`](crate::Session::download_url)) with its placeholders filled in,
/// asking for the bytes as `media_type` and naming the file `name` (RFC 8620 §6.2).
///
/// Every substitution is percent-encoded, so no value, a media type read from a server's
/// payload included, can add a path segment, a query parameter or a fragment.
///
/// # Examples
///
/// ```
/// use engine_core::ids::BlobId;
/// use provider_jmap::{JmapAccountId, blob::download_url};
///
/// let blob = BlobId::try_from("G3a7")?;
/// let url = download_url(
///     "https://jmap.example.com/download/{accountId}/{blobId}/{name}?type={type}",
///     &JmapAccountId::new("c"),
///     &blob,
///     "message/rfc822",
///     "message.eml",
/// );
/// assert_eq!(
///     url,
///     "https://jmap.example.com/download/c/G3a7/message.eml?type=message%2Frfc822"
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn download_url(
    template: &str,
    account: &JmapAccountId,
    blob: &BlobId,
    media_type: &str,
    name: &str,
) -> String {
    fill_download(template, account.as_str(), blob.as_str(), media_type, name)
}

/// The URL that uploads a blob into `account`: the session's `uploadUrl` `template`
/// ([`Session::upload_url`](crate::Session::upload_url)) with its `{accountId}` filled in,
/// percent-encoded as [`download_url`] encodes (RFC 8620 §6.1).
///
/// # Examples
///
/// ```
/// use provider_jmap::{JmapAccountId, blob::upload_url};
///
/// let url = upload_url(
///     "https://jmap.example.com/upload/{accountId}/",
///     &JmapAccountId::new("c"),
/// );
/// assert_eq!(url, "https://jmap.example.com/upload/c/");
/// ```
#[must_use]
pub fn upload_url(template: &str, account: &JmapAccountId) -> String {
    fill_upload(template, account.as_str())
}

/// [`download_url`] over ids the adapter holds as text.
pub(crate) fn fill_download(
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

/// [`upload_url`] over an account id the adapter holds as text.
pub(crate) fn fill_upload(template: &str, account: &str) -> String {
    template.replace("{accountId}", &encode_placeholder(account))
}

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
    crate::base64::decode(payload)
}

/// A blob being downloaded, read a chunk at a time so it is never whole in memory
/// ([`JmapClient::download_stream`](crate::JmapClient::download_stream)).
///
/// Each read waits at most the engine's stall bound
/// ([`Deadlines::stall`](engine_provider::Deadlines::stall)) for the server to send something,
/// so a server that stops mid-blob fails the read rather than parking it.
///
/// # Examples
///
/// ```no_run
/// use provider_jmap::{JmapClient, JmapError};
///
/// async fn blob_size(client: &JmapClient, url: &str) -> Result<u64, JmapError> {
///     let mut blob = client.download_stream(url).await?;
///     let mut size = 0;
///     while let Some(chunk) = blob.next_chunk().await? {
///         size += chunk.len() as u64;
///     }
///     Ok(size)
/// }
/// ```
pub struct BlobDownload {
    response: reqwest::Response,
}

impl BlobDownload {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self { response }
    }

    /// The blob's size in bytes, where the server said it (`Content-Length`).
    #[must_use]
    pub fn size(&self) -> Option<u64> {
        self.response.content_length()
    }

    /// The next piece of the blob, or `None` once it has all arrived.
    ///
    /// # Errors
    ///
    /// [`JmapError::Transport`] when the connection fails or the server sends nothing for the
    /// stall bound; the blob is then incomplete, and is fetched again from the start.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, JmapError> {
        let stall = engine_provider::Deadlines::STANDARD.stall();
        let chunk = engine_http::chunk_within(&mut self.response, stall).await?;
        Ok(chunk.map(|bytes| bytes.to_vec()))
    }
}

impl core::fmt::Debug for BlobDownload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlobDownload")
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
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
        blob,
        "application/octet-stream",
        "message",
    );
    let bytes = executor.download(&url).await?;
    Ok(RawMime::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::{fill_download as download_url, fill_upload};

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

    /// The upload template gets the same rule: before it, a draft's upload URL substituted the
    /// account raw, so an account id carrying a `/` or a `?` restructured it.
    #[test]
    fn a_hostile_account_cannot_restructure_the_upload_url() {
        let url = fill_upload("https://jmap.test/upload/{accountId}/", "../a?b#c");
        assert_eq!(url, "https://jmap.test/upload/..%2Fa%3Fb%23c/");
    }

    #[test]
    fn non_ascii_is_encoded_as_utf8_bytes() {
        let url = download_url(TEMPLATE, "acc", "blob", "image/jpeg", "café");
        assert!(url.contains("caf%C3%A9"), "{url}");
    }
}
