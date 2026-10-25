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
//!   collection home), each one allowed by TLS and by the host's
//!   [`CredentialHosts`](crate::CredentialHosts) policy when it was added. Only these may ever
//!   receive the credential.
//! - **answered**: trusted origins that receive the credential with every request. None starts
//!   here, the configured server included: an origin joins once it has asked, with a `401` whose
//!   `WWW-Authenticate` offers the scheme the credential is for, and is then sent it for as long as
//!   the connection lasts, the reuse RFC 9110 §11.5 allows "if a prior request has been
//!   authorized".
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

/// Whether a `401` carrying `headers` asks for a credential of `scheme`: one of its
/// `WWW-Authenticate` challenges names the scheme, or it carries no challenge at all.
///
/// A field value may carry several challenges, separated by commas that also separate each
/// challenge's parameters (RFC 9110 §11.6.1); a scheme is the element whose first token is not
/// a `name=value` parameter.
///
/// RFC 9110 §15.5.2 says a `401` MUST carry a challenge, and Yahoo's calendar server sends
/// none. A bare `401` from a trusted origin is still that server asking for credentials, so it
/// is answered; a `401` whose challenges all name another scheme is not, since that server has
/// said what it wants and it is not this credential.
fn offers(headers: &HeaderMap, scheme: &str) -> bool {
    let mut challenges = headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|element| element.split_whitespace().next())
        .filter(|token| !token.contains('='))
        .peekable();
    challenges.peek().is_none() || challenges.any(|token| token.eq_ignore_ascii_case(scheme))
}

#[cfg(test)]
#[path = "credential_scope_tests.rs"]
mod tests;
