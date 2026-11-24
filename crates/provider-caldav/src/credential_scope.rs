//! Which origins receive the account's credential, and when.
//!
//! HTTP scopes a credential to a **protection space**: an origin (scheme, host and port)
//! together with the realm its challenge names, and "a single protection space cannot extend
//! outside the scope of its server" (RFC 9110 §11.5). A redirect does not carry a credential
//! along either: `Authorization` is one of the header fields a client removes and recomputes
//! for the new request (RFC 9110 §15.4).
//!
//! So a connection keeps two sets of origins:
//!
//! - **trusted**: the origin the walk starts at (the host's configured URL) and every origin the
//!   account's own server led discovery to from there (a redirect target, the principal, the
//!   collection home), each one admitted when it was added: by the engine's plaintext rule, by the
//!   account's [`CredentialSource::permits`](engine_provider::CredentialSource::permits), and by
//!   the deprecated `CredentialHosts` policy where a host still sets one
//!   ([`AuthenticatedClient::admits`](engine_http::AuthenticatedClient::admits)). Only these may
//!   ever receive the credential.
//! - **answered**: trusted origins that receive the credential with every request. None starts
//!   here, the configured server included: an origin joins once it has asked, with a `401` whose
//!   `WWW-Authenticate` offers the scheme the credential is for, and is then sent it for as long as
//!   the connection lasts, the reuse RFC 9110 §11.5 allows "if a prior request has been
//!   authorized".
//!
//! A URL named by remote content, such as a vCard `PHOTO;VALUE=uri`, is never trusted, so it is
//! fetched without the credential even when it asks for one.
//!
//! A scope belongs to one connection, and a connection to one
//! [`CredentialSource`](engine_provider::CredentialSource) for its whole life, so a scope only
//! ever serves one subject: no origin one account's server answered is ever sent another
//! account's credential. Nothing here is keyed by subject explicitly; the shared endpoints of
//! `docs/design/credential-source.md` ("Direct host") will key it so.

use std::{collections::HashSet, sync::RwLock};

use engine_http::{asks_for, offered_schemes};
use reqwest::{Url, header::HeaderMap};

/// The origins a connection may send its credential to, and the ones it does.
#[derive(Debug)]
pub(crate) struct CredentialScope {
    trusted: RwLock<HashSet<String>>,
    answered: RwLock<HashSet<String>>,
}

impl CredentialScope {
    /// A scope that trusts nothing yet.
    pub(crate) fn empty() -> Self {
        Self {
            trusted: RwLock::new(HashSet::new()),
            answered: RwLock::new(HashSet::new()),
        }
    }

    /// A scope in which the configured server is trusted and nothing is answered yet.
    pub(crate) fn new(configured: &Url) -> Self {
        Self {
            trusted: RwLock::new(HashSet::from([origin_of(configured)])),
            answered: RwLock::new(HashSet::new()),
        }
    }

    /// Lets `url`'s origin receive the credential once it asks for it.
    pub(crate) fn trust(&self, url: &Url) {
        self.trusted
            .write()
            .expect("trusted lock")
            .insert(origin_of(url));
    }

    /// Whether a request to `url` carries the credential without being asked.
    pub(crate) fn sends_to(&self, url: &Url) -> bool {
        self.answered
            .read()
            .expect("answered lock")
            .contains(&origin_of(url))
    }

    /// Lets `url`'s origin receive the credential from now on although it never challenged,
    /// because it answered reporting the requester as `<DAV:unauthenticated/>` (RFC 5397 §3):
    /// how a server that lets anonymous requests through says it does not know who asked.
    /// Returns whether anything changed: `false` for an origin that is not trusted or is
    /// already answered.
    pub(crate) fn volunteer(&self, url: &Url) -> bool {
        let origin = origin_of(url);
        self.trusted.read().expect("trusted lock").contains(&origin)
            && self.answered.write().expect("answered lock").insert(origin)
    }

    /// Whether `url`'s origin may receive the credential once it asks.
    pub(crate) fn trusts(&self, url: &Url) -> bool {
        self.trusted
            .read()
            .expect("trusted lock")
            .contains(&origin_of(url))
    }

    /// The schemes a `401` from `url` carrying `headers` offers, when `url`'s origin is trusted
    /// and none of them is `scheme`, the one the account's credential is presented under: the
    /// server wants a method this account cannot use, so nothing was sent and nothing is wrong
    /// with the credential.
    pub(crate) fn unanswerable(
        &self,
        url: &Url,
        headers: &HeaderMap,
        scheme: &str,
    ) -> Option<String> {
        let offered = offered_schemes(headers);
        if !self.trusts(url) || asks_for(&offered, scheme) {
            return None;
        }
        Some(offered.join(", "))
    }

    /// Whether a `401` from `url` carrying `headers` is a challenge a credential presented under
    /// `scheme` should answer: the origin is trusted and offers the scheme. An origin that
    /// qualifies is answered from then on.
    pub(crate) fn answers(&self, url: &Url, headers: &HeaderMap, scheme: &str) -> bool {
        if !self.trusts(url) || !asks_for(&offered_schemes(headers), scheme) {
            return false;
        }
        self.answered
            .write()
            .expect("answered lock")
            .insert(origin_of(url));
        true
    }
}

/// The origin a scope keys on: scheme, host and effective port.
fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

#[cfg(test)]
#[path = "credential_scope_tests.rs"]
mod tests;
