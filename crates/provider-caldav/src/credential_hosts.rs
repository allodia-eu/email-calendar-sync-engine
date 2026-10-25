//! Which hosts may receive the account's credential, decided by the host application.
//!
//! Discovery follows a server's redirect to another origin and takes the credential along
//! ([`DavExecutor::trust_origin`](crate::transport::DavExecutor::trust_origin)), which is right
//! for the server the account was configured against and wrong for a host nobody vouched for. A
//! password is the host's to confine; an OAuth access token is more pressing, because whoever
//! receives one can use it wherever its issuer's resource servers accept it. Which hosts qualify
//! is a decision the host makes from the token's issuer, so the adapter asks rather than knows.

use std::sync::Arc;

/// Decides whether `host`, a lowercase host name with no port, may receive the credential.
///
/// Consulted for the configured base URL before anything is sent, and for every origin
/// discovery moves the connection to. A refusal fails the connect; the credential is never sent
/// to a host the policy refused.
pub trait CredentialHosts: Send + Sync {
    /// Whether `host` may receive the credential.
    fn permits(&self, host: &str) -> bool;
}

/// The host of `url` as a [`CredentialHosts`] policy sees it, or `None` when it names none.
pub(crate) fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// Whether `policy` lets `url` receive the credential: always, without a policy.
pub(crate) fn permitted(policy: Option<&Arc<dyn CredentialHosts>>, url: &str) -> bool {
    policy.is_none_or(|policy| host_of(url).is_some_and(|host| policy.permits(&host)))
}

/// Refuses a configured base URL the policy does not permit, before anything is sent to it.
pub(crate) fn refuse_unpermitted_base(
    policy: Option<&Arc<dyn CredentialHosts>>,
    base_url: &str,
) -> Result<(), crate::CalDavError> {
    if permitted(policy, base_url) {
        Ok(())
    } else {
        Err(crate::CalDavError::protocol(
            "the account's credential is not permitted at the configured server".to_owned(),
        ))
    }
}

#[cfg(test)]
#[path = "credential_hosts_tests.rs"]
mod credential_hosts_tests;
