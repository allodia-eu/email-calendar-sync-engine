//! How this crate presents an account's credential over HTTP, and reads a server's refusal of it.
//!
//! CalDAV, CardDAV and Talk all send the credential through
//! [`engine_http::AuthenticatedClient`], which asks the account's source for it on every request
//! (`docs/design/credential-source.md`). These are the two pieces that client leaves to the
//! adapter: how a credential goes on a request ([`present`]), and what a `401` to a request that
//! carried it says. Both are `engine_http`'s, shared with every adapter that takes the credential
//! as it stands ([`engine_http::judge_by_scheme`]); what is left here is the crate's own
//! credential type and the challenge no credential can answer.

use engine_http::asks_for;
use engine_provider::Credential;
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

/// `credential` on `builder`, in the scheme [`engine_http::credential_scheme`] names
/// ([`engine_http::present_credential`]).
///
/// # Errors
///
/// [`CalDavError::Credential`], `Permanent`, for a kind HTTP cannot carry. Nothing is sent.
pub(crate) fn present(
    builder: RequestBuilder,
    credential: &Credential,
) -> Result<RequestBuilder, CalDavError> {
    Ok(engine_http::present_credential(builder, credential)?)
}

/// Whether a `401` offering `offered` can be answered by no credential HTTP carries at all:
/// it names schemes, and Basic and Bearer are not among them (`Digest`, `Negotiate`, `NTLM`).
/// Decided before the source is asked, so a server no account can sign in to costs no token.
pub(crate) fn unanswerable_by_any(offered: &[String]) -> bool {
    !offered.is_empty() && !asks_for(offered, "basic") && !asks_for(offered, "bearer")
}
