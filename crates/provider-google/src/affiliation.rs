//! Whether the signed-in account is a personal Google account or a Workspace one:
//! `GET /oauth2/v3/userinfo`.
//!
//! A Workspace account's reply carries `hd`, the hosted domain; a personal account's has no such
//! field. Both were observed live with a token holding `userinfo.email` alone, and the field is
//! the same one Google documents on the ID token. A token without `userinfo.email` is answered
//! `401`, which says nothing about the account and stays an error.
//!
//! The organization is named by its hosted domain, the only name the reply gives it.

use engine_core::{affiliation::Affiliation, ids::OrganizationId};

use crate::{error::GoogleError, json::opt_str, transport::GoogleClient};

impl GoogleClient {
    /// Who the signed-in account belongs to.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GoogleError`] when Google did not answer: a token without
    /// `userinfo.email`, an expired one, a throttled or failed request.
    pub async fn affiliation(&self) -> Result<Affiliation, GoogleError> {
        let reply = self.get(&self.url("/oauth2/v3/userinfo")).await?;
        match opt_str(&reply, "hd").filter(|domain| !domain.is_empty()) {
            Some(domain) => OrganizationId::try_from(domain)
                .map(Affiliation::Organization)
                .map_err(|err| GoogleError::protocol(format!("hosted domain: {err}"))),
            None => Ok(Affiliation::Personal),
        }
    }
}

#[cfg(test)]
#[path = "affiliation_tests.rs"]
mod tests;
