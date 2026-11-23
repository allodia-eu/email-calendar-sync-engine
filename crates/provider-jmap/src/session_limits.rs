//! Limits from the JMAP session resource: the core ones, the mail account's (mailboxes per
//! email, and the blobs one `Blob/upload` takes), and calendar's per-event cap.

#[cfg(feature = "import")]
use std::num::NonZeroUsize;

use serde_json::Value;

use crate::{request::capability, session::Session};

/// Server limits the client must respect when batching (RFC 8620 §1.5 core
/// capability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CoreLimits {
    /// The largest blob the `uploadUrl` takes, in bytes (`maxSizeUpload`).
    pub max_size_upload: u64,
    /// Max uploads the server takes at once (`maxConcurrentUpload`).
    pub max_concurrent_upload: usize,
    /// The largest request the API endpoint takes, in bytes (`maxSizeRequest`). A request over
    /// it fails whole, so a batch carrying blobs inline is sized against it.
    pub max_size_request: u64,
    /// Max objects fetchable in a single `/get` (`maxObjectsInGet`).
    pub max_objects_in_get: usize,
    /// Max objects settable in a single `/set` (`maxObjectsInSet`).
    pub max_objects_in_set: usize,
    /// Max method calls in one request (`maxCallsInRequest`).
    pub max_calls_in_request: usize,
    /// Max requests the server will accept at once (`maxConcurrentRequests`).
    ///
    /// RFC 8620 §2 scopes this to the API endpoint, and defines no companion limit for
    /// the download endpoint — but a server is free to apply one number to both, and
    /// Stalwart does: exceeding it on a blob download is refused with a `400`
    /// `urn:ietf:params:jmap:error:limit`, not queued. So this is what bounds a
    /// concurrent body warm too. It defaults to `1` rather than to a guess, because the
    /// cost of being wrong is asymmetric — too narrow is slow, too wide is refused
    /// requests.
    ///
    /// Reading it rather than picking a number is what makes this right on both servers
    /// seen so far. Stalwart says 4, and a hosted deployment refuses the downloads beyond it
    /// with that `400`. Fastmail says 10 and queues rather than refuses, and paces an
    /// account's downloads at about 25 a second whatever the width: measured over 500
    /// uncached messages at each of 5, 10, 20, 50 and 100 in flight, on HTTP/2 and HTTP/1.1,
    /// the rate stayed at 25 and each request simply waited longer. So a width past the
    /// advertised number is refused on one server and buys nothing on the other.
    ///
    /// A Fastmail measurement taken through a download URL rebased onto the API origin is
    /// not a measurement of downloads: that origin answers `200` with an HTML page for the
    /// download path, which is quick. Fastmail serves blobs from an origin of its own, and this
    /// adapter downloads from exactly the URL Fastmail advertised: it rewrites a template onto
    /// the connection only when the template is on the API's own origin (`rebase_template`).
    pub max_concurrent_requests: usize,
}

impl Default for CoreLimits {
    fn default() -> Self {
        // Conservative RFC-floor-ish fallbacks if the server omits the core
        // capability (it never should). Keeps batching correct, just smaller. The sizes are
        // RFC 8620 §2's suggested minimums; each concurrency is one, as for requests.
        Self {
            max_size_upload: 50_000_000,
            max_concurrent_upload: 1,
            max_size_request: 10_000_000,
            max_objects_in_get: 100,
            max_objects_in_set: 100,
            max_calls_in_request: 16,
            max_concurrent_requests: 1,
        }
    }
}

pub(crate) fn parse_limits(core: &Value) -> CoreLimits {
    let defaults = CoreLimits::default();
    let read = |name: &str, fallback: usize| {
        core.get(name)
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|&value| value > 0)
            .unwrap_or(fallback)
    };
    let size = |name: &str, fallback: u64| {
        core.get(name)
            .and_then(Value::as_u64)
            .filter(|&value| value > 0)
            .unwrap_or(fallback)
    };
    CoreLimits {
        max_size_upload: size("maxSizeUpload", defaults.max_size_upload),
        max_concurrent_upload: read("maxConcurrentUpload", defaults.max_concurrent_upload),
        max_size_request: size("maxSizeRequest", defaults.max_size_request),
        max_objects_in_get: read("maxObjectsInGet", defaults.max_objects_in_get),
        max_objects_in_set: read("maxObjectsInSet", defaults.max_objects_in_set),
        max_calls_in_request: read("maxCallsInRequest", defaults.max_calls_in_request),
        max_concurrent_requests: read("maxConcurrentRequests", defaults.max_concurrent_requests),
    }
}

/// Whether the mail account takes blobs inside a request (RFC 9404 `Blob/upload`).
#[cfg(feature = "import")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobUpload {
    /// The account does not list the blob extension: every blob goes to the `uploadUrl`.
    Unsupported,
    /// `Blob/upload` takes a blob of up to `max_size` bytes (`maxSizeBlobSet`); `None` where
    /// the server states no limit.
    Supported { max_size: Option<u64> },
}

/// What the mail account states about the messages stored in it.
#[cfg(feature = "import")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MailLimits {
    /// `maxMailboxesPerEmail` (RFC 8621 §1.3.1); `None` where the server states no limit.
    pub(crate) max_mailboxes_per_email: Option<NonZeroUsize>,
    /// Whether, and how large, blobs go up inside a request.
    pub(crate) blob_upload: BlobUpload,
}

#[cfg(feature = "import")]
/// The [`MailLimits`] of `mail_account`, as `session` states them. `blob_extension` says whether
/// the server and that account both list RFC 9404.
pub(crate) fn mail_limits(
    session: &Value,
    mail_account: Option<&str>,
    blob_extension: bool,
) -> MailLimits {
    let account_caps =
        mail_account.and_then(|id| session.get("accounts")?.get(id)?.get("accountCapabilities"));
    let of = |urn: &str, name: &str| account_caps.and_then(|caps| caps.get(urn)?.get(name));
    let max_mailboxes_per_email = of(capability::MAIL, "maxMailboxesPerEmail")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .and_then(NonZeroUsize::new);
    let blob_upload = if blob_extension {
        BlobUpload::Supported {
            max_size: of(capability::BLOB, "maxSizeBlobSet").and_then(Value::as_u64),
        }
    } else {
        BlobUpload::Unsupported
    };
    MailLimits {
        max_mailboxes_per_email,
        blob_upload,
    }
}

pub(crate) fn max_participants_per_event(
    session: &Value,
    calendar_account_id: Option<&str>,
) -> Option<usize> {
    session
        .get("accounts")?
        .get(calendar_account_id?)?
        .get("accountCapabilities")?
        .get(capability::CALENDARS)?
        .get("maxParticipantsPerEvent")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

impl Session {
    pub(crate) fn max_participants_per_event(&self) -> Option<usize> {
        self.max_participants_per_event
    }

    /// What the mail account states about the messages stored in it.
    #[cfg(feature = "import")]
    pub(crate) fn mail_limits(&self) -> MailLimits {
        self.mail_limits
    }
}

#[cfg(test)]
#[path = "session_limits_tests.rs"]
mod tests;
