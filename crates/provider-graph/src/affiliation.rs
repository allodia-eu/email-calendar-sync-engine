//! Whether the signed-in account is a personal Microsoft account or a work or school one:
//! `GET /organization?$select=id`.
//!
//! Graph documents `/organization` for work or school accounts only, and both answers were
//! observed live: an organization's account gets its tenant back, a personal account is refused
//! with `400 BadRequest` ("This API is not supported for MSA accounts"). That refusal is the
//! answer, not a failure. The request needs only `User.Read`, which every Graph sign-in holds.
//!
//! Asked through the credential, never a mailbox principal: a shared mailbox belongs to the same
//! organization as the person who opens it.

use engine_core::{affiliation::Affiliation, ids::OrganizationId};

use crate::{error::GraphError, json::opt_str, transport::GraphClient};

impl GraphClient {
    /// Who the signed-in account belongs to.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] for anything but the two answers above: an expired
    /// token, a throttled or failed request, or an organization reply that names no tenant.
    pub async fn affiliation(&self) -> Result<Affiliation, GraphError> {
        let url = self.global_url("/organization?$select=id");
        match self.get(&url).await {
            Ok(reply) => {
                let id = reply
                    .get("value")
                    .and_then(|value| value.get(0))
                    .and_then(|organization| opt_str(organization, "id"))
                    .ok_or_else(|| GraphError::protocol("organization reply names no tenant"))?;
                let id = OrganizationId::try_from(id)
                    .map_err(|err| GraphError::protocol(format!("organization id: {err}")))?;
                Ok(Affiliation::Organization(id))
            }
            Err(GraphError::Status {
                status: 400,
                code: Some(code),
                ..
            }) if code == "BadRequest" => Ok(Affiliation::Personal),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
#[path = "affiliation_tests.rs"]
mod tests;
