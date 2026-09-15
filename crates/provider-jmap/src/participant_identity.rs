//! Selection of the authenticated account's scheduling identity.

use engine_core::scheduling::addresses_match;
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// Returns the participant map id belonging to `organiser`.
///
/// JMAP Calendars associates participants with account identities by calendar address. The
/// lookup also rejects an organiser address the authenticated account does not own.
pub(crate) async fn organiser_participant_id(
    executor: &dyn Executor,
    account: &str,
    organiser: &str,
) -> Result<String, JmapError> {
    let mut request = Request::new([capability::CORE, capability::CALENDARS]);
    let call = request.invoke(
        "ParticipantIdentity/get",
        json!({
            "accountId": account,
            "ids": null,
            "properties": ["id", "calendarAddress", "isDefault"],
        }),
    );
    let response = executor.execute(&request).await?;
    let result = response.result(&call)?;
    let identities = result
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| JmapError::protocol("ParticipantIdentity/get response has no list"))?;

    identities
        .iter()
        .find(|identity| {
            identity
                .get("calendarAddress")
                .and_then(Value::as_str)
                .is_some_and(|address| addresses_match(address, organiser))
        })
        .and_then(|identity| identity.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            JmapError::protocol(
                "the meeting organiser is not a participant identity of this JMAP account",
            )
        })
}
