//! How this crate presents an account's credential over HTTP, and reads a server's refusal of it.
//!
//! CalDAV, CardDAV and Talk all send the credential through
//! [`engine_http::AuthenticatedClient`], which asks the account's source for it on every request
//! (`docs/design/credential-source.md`). These are the two pieces that client leaves to the
//! adapter: how a credential goes on a request ([`present`]), and what a `401` to a request that
//! carried it says ([`judge`]). Both are WebDAV-neutral, so one answer serves all three.

use engine_http::{Sent, Unauthorized, asks_for, credential_scheme, offered_schemes};
use engine_provider::{Credential, ProviderError};
use reqwest::RequestBuilder;

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

/// `credential` on `builder`, in the scheme [`credential_scheme`] names.
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
    match credential_scheme(credential) {
        Some(scheme) if asks_for(&offered_schemes(reply.headers()), scheme) => {
            Unauthorized::Refused
        }
        _ => Unauthorized::Final,
    }
}

/// Whether a `401` offering `offered` can be answered by no credential HTTP carries at all:
/// it names schemes, and Basic and Bearer are not among them (`Digest`, `Negotiate`, `NTLM`).
/// Decided before the source is asked, so a server no account can sign in to costs no token.
pub(crate) fn unanswerable_by_any(offered: &[String]) -> bool {
    !offered.is_empty() && !asks_for(offered, "basic") && !asks_for(offered, "bearer")
}
