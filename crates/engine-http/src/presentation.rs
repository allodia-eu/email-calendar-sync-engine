//! How a credential goes on a request, and what a `401` to a request that carried it says, for
//! a server that takes the credential as it stands: Basic for a password (RFC 7617), Bearer for
//! a token (RFC 6750).
//!
//! These are the `present` and `judge` an adapter hands [`AuthenticatedClient::send`]: WebDAV
//! and Talk take either kind ([`present_credential`]), and an OAuth API that signs in with a
//! token alone takes only that ([`present_bearer`]). Both read a refusal the same way
//! ([`judge_by_scheme`]). JMAP, whose server may want a password framed as a token, keeps a
//! presentation of its own, built on the same [`credential_scheme`] and [`asks_for`].
//!
//! [`AuthenticatedClient::send`]: crate::AuthenticatedClient::send

use engine_provider::{Credential, CredentialFailure};
use reqwest::{RequestBuilder, header::HeaderMap};

use crate::{Sent, Unauthorized, asks_for, credential_scheme, offered_schemes};

/// `credential` on `builder`, under the scheme [`credential_scheme`] names: Basic for a
/// password, Bearer for a token.
///
/// # Errors
///
/// A `Permanent` [`CredentialFailure`] for a kind HTTP cannot carry: an administrator acting as
/// another account, since HTTP has no authorization identity to send it in. Nothing is sent.
pub fn present_credential(
    builder: RequestBuilder,
    credential: &Credential,
) -> Result<RequestBuilder, CredentialFailure> {
    match credential {
        Credential::Password { username, password } => {
            present_basic(builder, username, password.expose())
        }
        Credential::Bearer { token, .. } => present_token(builder, token.expose()),
        _ => Err(CredentialFailure::unpresentable(
            "this kind of credential cannot be presented over HTTP, which has no authorization \
             identity",
        )),
    }
}

/// `credential` on `builder` as a Bearer token, for an API that signs in with a token alone.
/// A token's `username` is not sent: the token names its account.
///
/// # Errors
///
/// A `Permanent` [`CredentialFailure`] for any other kind, a password included: such a server
/// has nothing to check a password against, so it is never sent there. Nothing is sent.
pub fn present_bearer(
    builder: RequestBuilder,
    credential: &Credential,
) -> Result<RequestBuilder, CredentialFailure> {
    match credential {
        Credential::Bearer { token, .. } => present_token(builder, token.expose()),
        _ => Err(CredentialFailure::unpresentable(
            "this server signs in only with an access token",
        )),
    }
}

/// `token` on `builder` as `Authorization: Bearer <token>` (RFC 6750 §2.1), checked first.
///
/// The check is the one place every HTTP adapter's token goes through. A token is a run of
/// visible ASCII (`0x21` to `0x7E`): that holds RFC 6750's `b64token` and the wider tokens real
/// issuers mint (`.`, `!`, `*`, `$` and the like), and refuses what a header cannot carry or
/// would read as something else: a control character (CR and LF would end the header), a space,
/// `DEL` or a byte past ASCII. An empty token is refused too.
///
/// # Errors
///
/// A `Permanent` [`CredentialFailure`] for a token outside that set, naming none of it. Nothing
/// is sent: a header the request builder cannot take would otherwise fail every send as a
/// transport error, retried for ever.
pub fn present_token(
    builder: RequestBuilder,
    token: &str,
) -> Result<RequestBuilder, CredentialFailure> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(CredentialFailure::unpresentable(
            "the access token holds a byte an Authorization header cannot carry",
        ));
    }
    Ok(builder.bearer_auth(token))
}

/// `username` and `password` on `builder` as `Authorization: Basic …` (RFC 7617 §2), checked
/// first: the user-id may not contain a colon, which would move the boundary between the two,
/// and neither may contain a control character. Any other byte goes, base64-encoded as UTF-8.
///
/// # Errors
///
/// A `Permanent` [`CredentialFailure`] for a username holding a colon, or either part holding a
/// control character, naming neither. Nothing is sent.
pub fn present_basic(
    builder: RequestBuilder,
    username: &str,
    password: &str,
) -> Result<RequestBuilder, CredentialFailure> {
    let control = |text: &str| text.chars().any(char::is_control);
    if username.contains(':') || control(username) || control(password) {
        return Err(CredentialFailure::unpresentable(
            "the username holds a colon, or the username or password a control character, \
             which Basic authentication cannot carry",
        ));
    }
    Ok(builder.basic_auth(username, Some(password)))
}

/// What a `401` to a request that carried `credential` says, read from its challenge
/// (`WWW-Authenticate`): the credential was [refused](Unauthorized::Refused) when the challenge
/// offers the scheme it went under, or offers none at all (YAHOO-1 in
/// `docs/spec-deviations.md`), and the source may replace it; a challenge only for other schemes
/// is [final](Unauthorized::Final), because another credential of the same kind would be
/// refused the same way.
#[must_use]
pub fn judge_by_scheme(reply: &Sent, credential: &Credential) -> Unauthorized {
    judge_challenge(reply.headers(), credential)
}

/// [`judge_by_scheme`] on the reply's headers.
fn judge_challenge(headers: &HeaderMap, credential: &Credential) -> Unauthorized {
    match credential_scheme(credential) {
        Some(scheme) if asks_for(&offered_schemes(headers), scheme) => Unauthorized::Refused,
        _ => Unauthorized::Final,
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
