//! [`with_credential`]: ask a source under a deadline, present the answer, and replace it once
//! if the server refuses it.

use core::{future::Future, time::Duration};

use super::{Credential, CredentialRequest, CredentialSource};
use crate::ProviderError;

/// The credential source failed, or the origin may not receive a credential, as opposed to the
/// server refusing one: the source did not permit the origin, or an engine rule such as HTTP's
/// plaintext rule refused it (nothing was sent); the source had no credential to give, or did
/// not answer in time; the credential is of a kind the adapter cannot present (nothing was
/// sent). Asked for a [replacement](super::CredentialSource::replace) after the
/// server refused a credential, its failure is this too, and then a request was sent and
/// refused: this failure takes the refusal's place.
///
/// An adapter turns it into its own error with `From`, keeping the class the source gave. The
/// class alone tells a host which failed and what to do: a broker that is down is
/// `Retryable`, and consent that is gone is `Authentication`, which is also what a server's
/// refusal is and needs the same answer.
#[derive(Debug)]
pub struct CredentialFailure(pub(super) ProviderError);

impl CredentialFailure {
    /// The engine refusing to present any credential to an origin, whatever the source would
    /// permit: [`Permanent`], with `reason`. For a rule no source can loosen, such as
    /// `engine_http`'s refusal of plaintext HTTP to anything but loopback.
    ///
    /// [`Permanent`]: engine_core::error::FailureClass::Permanent
    #[must_use]
    pub fn refused_origin(reason: &'static str) -> Self {
        Self(ProviderError::permanent(reason))
    }

    /// The adapter refusing to present a kind of credential its protocol cannot carry, such as
    /// a password to an API that signs in only with a token: [`Permanent`], with `reason`.
    /// Nothing is sent, and asking again gives the same kind.
    ///
    /// [`Permanent`]: engine_core::error::FailureClass::Permanent
    #[must_use]
    pub fn unpresentable(reason: &'static str) -> Self {
        Self(ProviderError::permanent(reason))
    }

    /// The source's error, with the class it gave.
    #[must_use]
    pub fn into_error(self) -> ProviderError {
        self.0
    }
}

impl core::fmt::Display for CredentialFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for CredentialFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<CredentialFailure> for ProviderError {
    fn from(failure: CredentialFailure) -> Self {
        failure.0
    }
}

/// Runs `attempt` with a credential from `source`, and once more with a replacement if the
/// server refused it and the source has one.
///
/// 1. The source must [permit](CredentialSource::permits) the request's origin. If it does not, the
///    attempt never runs and the result is a [`Permanent`] failure.
/// 2. The source is asked for a credential, and given `ask_limit` to answer. That is the deadline
///    of the step the ask belongs to: [`Deadlines::dial`](crate::Deadlines::dial) on IMAP and SMTP,
///    where the ask is part of the dial. A source that does not answer in time fails as
///    [`Retryable`], so a hung token endpoint cannot hang the caller.
/// 3. `attempt` runs with the credential. If it fails and `is_refusal` says the server refused the
///    credential, the source is asked for a [replacement](CredentialSource::replace), under the
///    same limit, and `attempt` runs once more with it. A second refusal is returned as it came: a
///    fresh credential that is also refused is an answer about the account, not about the token.
///    Without a replacement, the first refusal is returned.
///
/// `is_refusal` is the adapter's, and narrow: only the server refusing the credential
/// presented, before anything else happened, because a replacement repeats the whole attempt.
/// A source's own failures reach the caller through `E: From<CredentialFailure>` with the
/// source's class unchanged.
///
/// # Errors
///
/// The attempt's error, or the source's failure converted into `E`.
///
/// # Panics
///
/// Outside a tokio runtime with the timer enabled (`enable_time`, or `enable_all`), as
/// [`tokio::time::timeout`] does: the ask's deadline runs on the caller's runtime.
///
/// [`Permanent`]: engine_core::error::FailureClass::Permanent
/// [`Retryable`]: engine_core::error::FailureClass::Retryable
pub async fn with_credential<T, E, F, Fut>(
    source: &dyn CredentialSource,
    request: &CredentialRequest<'_>,
    ask_limit: Duration,
    is_refusal: impl Fn(&E) -> bool,
    mut attempt: F,
) -> Result<T, E>
where
    E: From<CredentialFailure>,
    F: FnMut(Credential) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    if !source.permits(request.origin) {
        return Err(CredentialFailure(ProviderError::permanent(
            "the credential source does not permit this origin",
        ))
        .into());
    }
    let credential = ask(ask_limit, source.credential(request)).await?;
    match attempt(credential.clone()).await {
        Err(refusal) if is_refusal(&refusal) => {
            match ask(ask_limit, source.replace(&credential, request)).await? {
                Some(replacement) => attempt(replacement).await,
                None => Err(refusal),
            }
        }
        outcome => outcome,
    }
}

/// Awaits one answer from a source, for no longer than `limit`.
async fn ask<T>(
    limit: Duration,
    answer: impl Future<Output = Result<T, ProviderError>>,
) -> Result<T, CredentialFailure> {
    match tokio::time::timeout(limit, answer).await {
        Ok(answer) => answer.map_err(CredentialFailure),
        Err(_) => Err(CredentialFailure(ProviderError::retryable(format!(
            "the credential source did not answer within {}s",
            limit.as_secs()
        )))),
    }
}
