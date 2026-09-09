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
/// single post-connect seam. Small and `Copy`, so it is returned by value and a provider
/// may compute it per call (an HTTP adapter reads a version its transport recorded on the
/// first response it saw).
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
/// // And it is one connection, so a caller must fetch over it one object at a time.
/// assert_eq!(info.concurrent_fetches, 1);
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
    /// How many single-object fetches a caller may usefully keep in flight against this
    /// connection at once. Always at least 1.
    ///
    /// A caller draining a work list one object at a time — warming bodies, resolving a
    /// list of ids — needs to know whether overlapping those fetches would help or merely
    /// contend. The answer is a property of the *transport*, not of the provider's name,
    /// which is why it is reported here rather than left to callers to infer
    /// (`providers.md`: read facts from this seam, never switch on provider kind).
    ///
    /// `1` for a session protocol whose commands share one socket (IMAP), so a fan-out
    /// would queue behind itself. Higher for an HTTP transport, where requests multiplex
    /// over a pooled HTTP/2 connection and the limit is whatever the *service* tolerates —
    /// so an adapter sets this from its own provider's documented or measured ceiling, not
    /// from a number that sounds safe.
    pub concurrent_fetches: usize,
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
            concurrent_fetches: 1,
        }
    }

    /// The same connection, reporting that `n` single-object fetches may be in flight at
    /// once. Clamped to at least 1, so a zero can never stall a caller's drain loop.
    #[must_use]
    pub const fn with_concurrent_fetches(mut self, n: usize) -> Self {
        self.concurrent_fetches = if n == 0 { 1 } else { n };
        self
    }
}

/// Whether `url` sits on the same origin — scheme, host, and effective port — as
/// `base`.
///
/// This is the rule every HTTP adapter applies **before it authenticates**. Some URLs
/// an adapter fetches come from remote *content* rather than from a server-issued
/// endpoint — a vCard `PHOTO;VALUE=uri`, a JSContact resource `uri`, a People
/// `photos[].url` — and such a URL may name any host. Attaching the account's
/// `Authorization` header unconditionally would hand the user's password (CardDAV
/// Basic) or OAuth token to whoever the payload names, so credentials travel only to
/// the origin the account is configured against. Anything else is fetched
/// anonymously: a public photo CDN still works, an attacker-named host learns nothing.
///
/// Returns `false` when either side fails to parse, so an unparseable URL is never
/// treated as trusted.
///
/// # Examples
///
/// ```
/// use engine_provider::same_origin;
///
/// // Same scheme/host/port — the account's own server.
/// assert!(same_origin(
///     "https://dav.example.com/photo.png",
///     "https://dav.example.com/addressbooks/u/"
/// ));
/// // A host named by card content is a different origin.
/// assert!(!same_origin(
///     "https://attacker.test/p.png",
///     "https://dav.example.com/addressbooks/u/"
/// ));
/// ```
#[cfg(feature = "http")]
#[must_use]
pub fn same_origin(url: &str, base: &str) -> bool {
    let (Ok(url), Ok(base)) = (url::Url::parse(url), url::Url::parse(base)) else {
        return false;
    };
    url.scheme() == base.scheme()
        && url.host() == base.host()
        && url.port_or_known_default() == base.port_or_known_default()
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

    #[cfg(feature = "http")]
    #[test]
    fn same_origin_matches_only_scheme_host_and_effective_port() {
        let base = "https://dav.example.com/addressbooks/user/";
        // Same origin, different path/query — the account's own server.
        assert!(super::same_origin(
            "https://dav.example.com/a/b.png?x=1",
            base
        ));
        // An explicit default port is still the same origin as an implicit one.
        assert!(super::same_origin(
            "https://dav.example.com:443/p.png",
            base
        ));
        // Foreign host — the attack this guard exists to stop.
        assert!(!super::same_origin("https://attacker.test/p.png", base));
        // A subdomain is a distinct host, not a relaxed match.
        assert!(!super::same_origin(
            "https://evil.dav.example.com/p.png",
            base
        ));
        // A host that merely *starts with* the base host must not match: the check is
        // host equality, never a prefix/substring test.
        assert!(!super::same_origin(
            "https://dav.example.com.evil.test/p",
            base
        ));
        // A downgraded scheme would leak the credential in cleartext.
        assert!(!super::same_origin("http://dav.example.com/p.png", base));
        // A non-default port is a different origin.
        assert!(!super::same_origin(
            "https://dav.example.com:8443/p.png",
            base
        ));
        // http://…:80 and http://… are the same origin, but https://…:80 is not.
        assert!(super::same_origin("http://h.test:80/a", "http://h.test/b"));
        assert!(!super::same_origin(
            "https://h.test:80/a",
            "https://h.test/b"
        ));
    }

    #[cfg(feature = "http")]
    #[test]
    fn same_origin_never_trusts_an_unparseable_url() {
        let base = "https://dav.example.com/";
        // A relative href has already been resolved against the base by the caller; an
        // unresolved one must not be treated as same-origin by accident.
        assert!(!super::same_origin("/photo.png", base));
        assert!(!super::same_origin("not a url", base));
        assert!(!super::same_origin(
            "https://dav.example.com/p.png",
            "not a url"
        ));
        // A `data:` URI carries no host — it is decoded inline, never fetched.
        assert!(!super::same_origin("data:image/png;base64,AAAA", base));
    }
}
