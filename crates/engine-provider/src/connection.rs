//! What an adapter learned about the connection it established.
//!
//! [`Capabilities`] answers "what can this account do?"; [`ConnectionInfo`] answers
//! the wider question "what do we know about this connection now that it is up?" —
//! the capabilities *plus* the transport versions that were negotiated. It is the
//! one post-connect fact object a host reads (`providers.md`), so a host that wants
//! to show or log "IMAP over TLS 1.3" / "JMAP over HTTP/2" makes one call and
//! switches on no provider kind.
//!
//! The two version fields are **independently optional**, because the facts a
//! provider can observe are asymmetric (`docs/agent-guidance/tls.md`):
//!
//! - A `tokio-rustls` provider (IMAP/SMTP) knows its TLS version and has no HTTP version.
//! - A `reqwest` provider (JMAP/CalDAV/Graph) knows its HTTP version and **cannot** learn its TLS
//!   version: reqwest exposes only the peer certificate, never the negotiated protocol version.
//!
//! The TLS *policy* is deliberately absent: the host chose it and already knows it
//! (`docs/agent-guidance/tls.md`). This object carries only what the **server**
//! decided.

use crate::Capabilities;

/// The TLS protocol version negotiated on a provider's connection.
///
/// Only the versions rustls implements — the engine's shared config pins a TLS 1.2
/// floor (`docs/agent-guidance/tls.md`), so nothing older can be negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TlsVersion {
    /// TLS 1.2 (RFC 5246).
    Tls1_2,
    /// TLS 1.3 (RFC 8446).
    Tls1_3,
}

/// The HTTP protocol version negotiated on a provider's connection.
///
/// The engine's shared reqwest client advertises ALPN `h2` then `http/1.1`, so an
/// HTTP provider gets [`Http2`](Self::Http2) where the server supports it and falls
/// back to [`Http1_1`](Self::Http1_1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpVersion {
    /// HTTP/1.1 (RFC 9112).
    Http1_1,
    /// HTTP/2 (RFC 9113).
    Http2,
}

#[cfg(feature = "http")]
impl HttpVersion {
    /// Maps an [`http::Version`] — what `reqwest::Response::version` returns — onto
    /// the two versions the engine's HTTP providers can negotiate, or `None` for any
    /// other version.
    ///
    /// Defined once here so the three HTTP adapters cannot drift on which versions
    /// they recognize. HTTP/0.9, HTTP/1.0, and HTTP/3 map to `None`: the shared
    /// client never negotiates them (its ALPN offers only `h2` and `http/1.1`), so a
    /// response claiming one is a fact the engine does not model rather than an error
    /// worth failing a sync over.
    #[must_use]
    pub fn from_http(version: http::Version) -> Option<Self> {
        match version {
            http::Version::HTTP_11 => Some(Self::Http1_1),
            http::Version::HTTP_2 => Some(Self::Http2),
            _ => None,
        }
    }

    /// The wire encoding [`ObservedHttpVersion`] stores. Never zero — zero means
    /// "nothing observed yet".
    const fn as_u8(self) -> u8 {
        match self {
            Self::Http1_1 => 1,
            Self::Http2 => 2,
        }
    }

    /// The inverse of [`as_u8`](Self::as_u8); `None` for the "nothing observed" zero.
    const fn from_u8(encoded: u8) -> Option<Self> {
        match encoded {
            1 => Some(Self::Http1_1),
            2 => Some(Self::Http2),
            _ => None,
        }
    }
}

/// The HTTP version most recently observed on one transport's connection.
///
/// Each `reqwest`-backed adapter holds one and records every response through its
/// single send/collect funnel; [`ConnectionInfo::http_version`] reads it. Shared here
/// so the three adapters cannot drift on the semantics.
///
/// **Most recent wins, not first.** A first-observation latch would report the wrong
/// endpoint: JMAP and CalDAV both disable reqwest's redirect following and resolve the
/// RFC 6764 / well-known `30x` themselves, so the *first* response a transport sees is
/// the redirector's — which may be a different origin, and a different negotiated
/// version, from the API or calendar home that then serves every real request. Taking
/// the latest instead makes the fact self-correcting: it always describes the exchange
/// the adapter most recently had.
///
/// A version the engine does not model (HTTP/3, HTTP/1.0 — never negotiated by the
/// shared client's ALPN) leaves the previous observation intact rather than erasing it.
#[cfg(feature = "http")]
#[derive(Debug, Default)]
pub struct ObservedHttpVersion(core::sync::atomic::AtomicU8);

#[cfg(feature = "http")]
impl ObservedHttpVersion {
    /// Records the version of a response just received, if the engine models it.
    ///
    /// `Relaxed` suffices: the value is a standalone diagnostic fact that publishes no
    /// other memory, and a racing pair of responses may land in either order — both are
    /// truthful answers to "what did this connection most recently speak?".
    pub fn record(&self, version: http::Version) {
        if let Some(modeled) = HttpVersion::from_http(version) {
            self.0
                .store(modeled.as_u8(), core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The most recently observed version, or `None` before any response.
    #[must_use]
    pub fn get(&self) -> Option<HttpVersion> {
        HttpVersion::from_u8(self.0.load(core::sync::atomic::Ordering::Relaxed))
    }
}

/// Everything an adapter learned about its connection once it was established: the
/// data domains the account exposes, and the transport versions the server
/// negotiated.
///
/// Returned by [`Provider::connection_info`](crate::Provider::connection_info) — the
/// single post-connect seam. Copy and three words wide, so it is returned by value
/// and a provider may compute it per call (an HTTP adapter reads a version its
/// transport recorded on the first response it saw).
///
/// ```
/// use engine_provider::{Capabilities, ConnectionInfo, TlsVersion};
///
/// let info = ConnectionInfo {
///     tls_version: Some(TlsVersion::Tls1_3),
///     ..ConnectionInfo::new(Capabilities::none().with_mail())
/// };
/// assert!(info.capabilities.mail());
/// assert_eq!(info.tls_version, Some(TlsVersion::Tls1_3));
/// // An IMAP connection has no HTTP version — the field is not applicable, not unset.
/// assert_eq!(info.http_version, None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The data domains this adapter supports.
    pub capabilities: Capabilities,
    /// The negotiated TLS version, or `None` when the transport cannot report one —
    /// either because the connection is not TLS at all, or because it is a `reqwest`
    /// provider (reqwest never exposes the negotiated version).
    pub tls_version: Option<TlsVersion>,
    /// The HTTP version most recently negotiated on this connection, or `None` for a
    /// non-HTTP provider (IMAP/SMTP) or an HTTP provider that has not yet exchanged a
    /// response. See [`ObservedHttpVersion`] for why it is the latest, not the first.
    pub http_version: Option<HttpVersion>,
}

impl ConnectionInfo {
    /// The capabilities alone, with no transport version facts — what a non-network
    /// adapter (and every offline test fake) reports.
    #[must_use]
    pub const fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            tls_version: None,
            http_version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_reports_capabilities_and_no_transport_facts() {
        // The shape every non-network adapter reports: capabilities, nothing else.
        let info = ConnectionInfo::new(Capabilities::none().with_mail());
        assert!(info.capabilities.mail());
        assert_eq!(info.tls_version, None);
        assert_eq!(info.http_version, None);
    }

    #[test]
    fn the_two_version_facts_are_independent() {
        // The asymmetry this type exists to model: a tokio-rustls provider reports a
        // TLS version and no HTTP version; a reqwest provider reports the reverse.
        let caps = Capabilities::none().with_mail();
        let imap = ConnectionInfo {
            tls_version: Some(TlsVersion::Tls1_3),
            ..ConnectionInfo::new(caps)
        };
        assert_eq!(imap.tls_version, Some(TlsVersion::Tls1_3));
        assert_eq!(imap.http_version, None);

        let jmap = ConnectionInfo {
            http_version: Some(HttpVersion::Http2),
            ..ConnectionInfo::new(caps)
        };
        assert_eq!(jmap.tls_version, None);
        assert_eq!(jmap.http_version, Some(HttpVersion::Http2));
    }

    #[cfg(feature = "http")]
    #[test]
    fn http_version_maps_only_the_two_versions_alpn_can_negotiate() {
        assert_eq!(
            HttpVersion::from_http(http::Version::HTTP_11),
            Some(HttpVersion::Http1_1)
        );
        assert_eq!(
            HttpVersion::from_http(http::Version::HTTP_2),
            Some(HttpVersion::Http2)
        );
        // The shared client's ALPN offers only `h2`/`http/1.1`, so anything else is a
        // version the engine does not model — reported as unknown, never as an error.
        for unmodeled in [
            http::Version::HTTP_09,
            http::Version::HTTP_10,
            http::Version::HTTP_3,
        ] {
            assert_eq!(HttpVersion::from_http(unmodeled), None);
        }
    }

    #[cfg(feature = "http")]
    #[test]
    fn nothing_is_observed_before_the_first_response() {
        assert_eq!(ObservedHttpVersion::default().get(), None);
    }

    #[cfg(feature = "http")]
    #[test]
    fn the_most_recent_observation_wins_not_the_first() {
        // The invariant that makes the fact correct for JMAP/CalDAV: both follow the
        // well-known `30x` themselves, so the FIRST response is the redirector's — a
        // possibly different origin, with a possibly different negotiated version, from
        // the endpoint that then serves every real request. Latching the first would
        // permanently misreport it.
        let observed = ObservedHttpVersion::default();
        observed.record(http::Version::HTTP_11);
        assert_eq!(observed.get(), Some(HttpVersion::Http1_1));
        observed.record(http::Version::HTTP_2);
        assert_eq!(observed.get(), Some(HttpVersion::Http2));
    }

    #[cfg(feature = "http")]
    #[test]
    fn an_unmodeled_version_leaves_the_last_observation_intact() {
        // Downgrading a known fact to "unknown" because one hop reported HTTP/3 would
        // lose information; the previous observation is still the better answer.
        let observed = ObservedHttpVersion::default();
        observed.record(http::Version::HTTP_2);
        observed.record(http::Version::HTTP_3);
        assert_eq!(observed.get(), Some(HttpVersion::Http2));
    }
}
