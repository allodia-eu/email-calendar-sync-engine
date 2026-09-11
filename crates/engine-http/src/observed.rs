//! What the shared send path observed about the connection it used.
//!
//! An HTTP adapter reports two transport facts through
//! [`ConnectionInfo`](engine_provider::ConnectionInfo) — the negotiated HTTP version
//! and the negotiated TLS version — and both arrive the same way: on a response that
//! came back through [`send_retrying`](crate::send_retrying). Recording them together
//! is the point of this type. They were never going to be observed at different
//! moments, and the four transports have between one and three send funnels each, so a
//! pair of separate `record` calls is a pair one of those funnels eventually forgets.
//!
//! The translation from reqwest's types to the engine's neutral ones lives here and
//! only here. `engine-provider` states the provider-neutral contract and deliberately
//! does not name a concrete HTTP client — `HttpVersion::from_http` takes the leaf
//! `http::Version` for that reason — so the crate that already wraps reqwest's send is
//! the one that owns reading a reqwest response.

use engine_provider::{HttpVersion, ObservedHttpVersion, ObservedTlsVersion, TlsVersion};

/// Maps reqwest's negotiated TLS version onto the engine's neutral [`TlsVersion`].
///
/// Anything but TLS 1.2/1.3 is `None`, for the same reason `provider-imap` maps the
/// rustls version that way: the shared config pins a TLS 1.2 floor and rustls
/// implements nothing newer than 1.3 (`docs/agent-guidance/tls.md`), so the older
/// constants cannot be negotiated. An unmodeled version is reported as unknown rather
/// than failing a request the TLS stack itself accepted.
fn from_reqwest(version: reqwest::tls::Version) -> Option<TlsVersion> {
    // `reqwest::tls::Version` is an opaque struct over a `#[non_exhaustive]` inner
    // enum, exposed only as associated constants, so this compares rather than matches.
    if version == reqwest::tls::Version::TLS_1_2 {
        Some(TlsVersion::Tls1_2)
    } else if version == reqwest::tls::Version::TLS_1_3 {
        Some(TlsVersion::Tls1_3)
    } else {
        None
    }
}

/// The TLS version negotiated for the connection `response` arrived on, or `None` for a
/// cleartext hop.
///
/// Reads reqwest's `TlsInfo` response extension, which is populated only because
/// `engine-tls`'s `TlsClientConfig::reqwest_builder` turns `tls_info` on for every
/// HTTP provider. A client built without that switch
/// reports `None` here forever — which is why the switch lives in the shared builder
/// and not in an adapter.
fn negotiated_tls_version(response: &reqwest::Response) -> Option<TlsVersion> {
    response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(reqwest::tls::TlsInfo::version)
        .and_then(from_reqwest)
}

/// The transport facts one HTTP adapter's connection has shown so far.
///
/// Each reqwest-backed adapter holds one and calls [`record`](Self::record) on every
/// response its send funnels return; `ConnectionInfo` then reads the two getters. Both
/// facts are "most recent wins, not first" — see
/// [`ObservedHttpVersion`] for why that is the correct rule for a transport that
/// follows its own well-known redirect.
///
/// Empty until the first response: an adapter whose `connect` performs no request
/// (Graph, Google) reports both as `None` until its first fetch.
#[derive(Debug, Default)]
pub struct ObservedConnection {
    http: ObservedHttpVersion,
    tls: ObservedTlsVersion,
}

impl ObservedConnection {
    /// Records both transport facts carried by a response just received.
    pub fn record(&self, response: &reqwest::Response) {
        self.http.record(response.version());
        self.tls.record(negotiated_tls_version(response));
    }

    /// The HTTP version most recently observed, or `None` before the first response.
    #[must_use]
    pub fn http_version(&self) -> Option<HttpVersion> {
        self.http.get()
    }

    /// The TLS version most recently observed, or `None` before the first response over
    /// TLS.
    #[must_use]
    pub fn tls_version(&self) -> Option<TlsVersion> {
        self.tls.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_versions_the_shared_config_can_negotiate_map() {
        assert_eq!(
            from_reqwest(reqwest::tls::Version::TLS_1_2),
            Some(TlsVersion::Tls1_2)
        );
        assert_eq!(
            from_reqwest(reqwest::tls::Version::TLS_1_3),
            Some(TlsVersion::Tls1_3)
        );
        // Below the shared config's TLS 1.2 floor — rustls will not negotiate these, so
        // they are reported as unknown rather than modeled.
        for below_floor in [
            reqwest::tls::Version::TLS_1_0,
            reqwest::tls::Version::TLS_1_1,
        ] {
            assert_eq!(from_reqwest(below_floor), None);
        }
    }

    #[test]
    fn nothing_is_observed_before_the_first_response() {
        let observed = ObservedConnection::default();
        assert_eq!(observed.http_version(), None);
        assert_eq!(observed.tls_version(), None);
    }
}
