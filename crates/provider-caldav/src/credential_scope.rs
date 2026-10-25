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
//! - **trusted**: origins the account's own server led discovery to (the configured server, a
//!   redirect target, the principal, the collection home), each one allowed by TLS and by the
//!   host's [`CredentialHosts`](crate::CredentialHosts) policy when it was added. Only these may
//!   ever receive the credential.
//! - **answered**: trusted origins that receive the credential with every request. The configured
//!   server starts here, since it is the server the person signed in to; any other trusted origin
//!   joins only once it has asked, with a `401` whose `WWW-Authenticate` offers the scheme the
//!   credential is for.
//!
//! A URL named by remote content, such as a vCard `PHOTO;VALUE=uri`, is never trusted, so it is
//! fetched without the credential even when it asks for one.

use std::{collections::HashSet, sync::RwLock};

use reqwest::{
    Url,
    header::{HeaderMap, WWW_AUTHENTICATE},
};

use crate::transport::Credentials;

/// The origins a connection may send its credential to, and the ones it does.
#[derive(Debug)]
pub(crate) struct CredentialScope {
    trusted: RwLock<HashSet<String>>,
    answered: RwLock<HashSet<String>>,
}

impl CredentialScope {
    /// A scope in which the configured server is trusted and answered.
    pub(crate) fn new(configured: &Url) -> Self {
        let origin = origin_of(configured);
        Self {
            trusted: RwLock::new(HashSet::from([origin.clone()])),
            answered: RwLock::new(HashSet::from([origin])),
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

    /// Whether a `401` from `url` carrying `headers` is a challenge `credentials` should answer:
    /// the origin is trusted and offers the credential's scheme. An origin that qualifies is
    /// answered from then on.
    pub(crate) fn answers(
        &self,
        url: &Url,
        headers: &HeaderMap,
        credentials: &Credentials,
    ) -> bool {
        let origin = origin_of(url);
        if !self.trusted.read().expect("trusted lock").contains(&origin)
            || !offers(headers, scheme_of(credentials))
        {
            return false;
        }
        self.answered.write().expect("answered lock").insert(origin);
        true
    }
}

/// The origin a scope keys on: scheme, host and effective port.
fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// The authentication scheme (RFC 9110 §11.1) `credentials` are presented with.
const fn scheme_of(credentials: &Credentials) -> &'static str {
    match credentials {
        Credentials::Basic { .. } => "basic",
        Credentials::Bearer(_) => "bearer",
    }
}

/// Whether any `WWW-Authenticate` challenge in `headers` names `scheme`.
///
/// A field value may carry several challenges, separated by commas that also separate each
/// challenge's parameters (RFC 9110 §11.6.1); a scheme is the element whose first token is not
/// a `name=value` parameter.
fn offers(headers: &HeaderMap, scheme: &str) -> bool {
    headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|element| element.split_whitespace().next())
        .filter(|token| !token.contains('='))
        .any(|token| token.eq_ignore_ascii_case(scheme))
}

#[cfg(test)]
#[path = "credential_scope_tests.rs"]
mod tests;
