//! How this crate presents an account's credential over HTTP, and reads a server's refusal of it.
//!
//! CalDAV, CardDAV and Talk all send the credential through
//! [`engine_http::AuthenticatedClient`], which asks the account's source for it on every request
//! (`docs/design/credential-source.md`). These are the two pieces that client leaves to the
//! adapter: how a credential goes on a request ([`present`]), and what a `401` to a request that
//! carried it says ([`judge`]). Both are WebDAV-neutral, so one answer serves all three.

use engine_http::{Sent, Unauthorized, challenge_schemes};
use engine_provider::{Credential, ProviderError};
use reqwest::{
    RequestBuilder,
    header::{HeaderMap, WWW_AUTHENTICATE},
};

use crate::{error::CalDavError, transport::Credentials};

/// A fixed credential in the crate's own type, as the neutral one.
impl From<Credentials> for Credential {
    fn from(credentials: Credentials) -> Self {
        match credentials {
            Credentials::Basic { username, password } => Self::password(username, password),
            Credentials::Bearer(token) => Self::bearer(token),
        }
    }
}

/// The authentication scheme (RFC 9110 §11.1) `credential` is presented under: Basic for a
/// password (RFC 7617), Bearer for a token (RFC 6750). `None` for a kind HTTP cannot carry: an
/// administrator acting as another account has no authorization identity to send it in.
pub(crate) fn scheme_of(credential: &Credential) -> Option<&'static str> {
    match credential {
        Credential::Password { .. } => Some("basic"),
        Credential::Bearer { .. } => Some("bearer"),
        _ => None,
    }
}

/// `credential` on `builder`, in the scheme [`scheme_of`] names.
///
/// # Errors
///
/// [`CalDavError::Credential`], `Permanent`, for a kind HTTP cannot carry. Nothing is sent.
pub(crate) fn present(
    builder: RequestBuilder,
    credential: &Credential,
) -> Result<RequestBuilder, CalDavError> {
    match credential {
        Credential::Password { username, password } => {
            Ok(builder.basic_auth(username, Some(password.expose())))
        }
        Credential::Bearer { token, .. } => Ok(builder.bearer_auth(token.expose())),
        _ => Err(unpresentable()),
    }
}

/// The failure for a credential HTTP cannot carry.
pub(crate) fn unpresentable() -> CalDavError {
    CalDavError::Credential(ProviderError::permanent(
        "this kind of credential cannot be presented over HTTP, which has no authorization \
         identity",
    ))
}

/// What a `401` to a request that carried `credential` says: the credential was refused when
/// the challenge offers its scheme, or offers none at all (YAHOO-1 in `docs/spec-deviations.md`),
/// and the source may replace it; a challenge only for other schemes is final, because another
/// credential of the same kind would be refused the same way.
pub(crate) fn judge(reply: &Sent, credential: &Credential) -> Unauthorized {
    match scheme_of(credential) {
        Some(scheme) if offers(reply.headers(), scheme) => Unauthorized::Refused,
        _ => Unauthorized::Final,
    }
}

/// Whether a `401` carrying `headers` asks for a credential of `scheme`: one of its
/// `WWW-Authenticate` challenges names the scheme, or it carries no challenge at all.
///
/// RFC 9110 §15.5.2 says a `401` MUST carry a challenge, and Yahoo's, Google's and AOL's calendar
/// servers send none (`docs/spec-deviations.md`, YAHOO-1). A bare `401` is still that server
/// asking for credentials; a `401` whose challenges all name another scheme is not, since that
/// server has said what it wants and it is not this credential.
pub(crate) fn offers(headers: &HeaderMap, scheme: &str) -> bool {
    let offered = offered(headers);
    offered.is_empty()
        || offered
            .iter()
            .any(|token| token.eq_ignore_ascii_case(scheme))
}

/// The authentication schemes the `WWW-Authenticate` challenges in `headers` name, in order
/// and as the server spelled them. A response may carry several fields (RFC 9110 §5.3), and
/// Fastmail and Stalwart send more than one.
pub(crate) fn offered(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .flat_map(|value| {
            challenge_schemes(&String::from_utf8_lossy(value.as_bytes()))
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
