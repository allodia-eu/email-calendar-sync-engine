//! `ParticipantIdentity/get`: the addresses the calendar account takes part in events as.
//!
//! JMAP Calendars keeps these as `ParticipantIdentity` objects on the calendars account, each
//! carrying the address the server schedules as for this user. They are the JMAP
//! counterpart of a CalDAV principal's `calendar-user-address-set`. An identity is shaped
//! like a JSCalendar participant, so its address is read the way a participant's is
//! ([`participant_address`]): `calendarAddress`, or the `sendTo` map of an earlier draft.

use engine_provider::CalendarUserAddresses;
use serde_json::{Value, json};

use crate::{
    calendar::participant_address,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// Reads the calendar account's participant identities as the addresses it schedules as.
///
/// A server that does not implement the method answers `unknownMethod`, which is
/// [`CalendarUserAddresses::Unknown`] rather than an error: it says nothing about the user.
pub(crate) async fn calendar_user_addresses(
    executor: &dyn Executor,
    calendar_account: &str,
) -> Result<CalendarUserAddresses, JmapError> {
    let mut req = Request::new([capability::CORE, capability::CALENDARS]);
    let call = req.invoke(
        "ParticipantIdentity/get",
        json!({ "accountId": calendar_account }),
    );
    let resp = executor.execute(&req).await?;
    match resp.result(&call) {
        Ok(result) => from_get(result),
        Err(JmapError::Method { error_type, .. }) if error_type == "unknownMethod" => {
            Ok(CalendarUserAddresses::Unknown)
        }
        Err(err) => Err(err),
    }
}

/// The addresses in a `ParticipantIdentity/get` result.
fn from_get(result: &Value) -> Result<CalendarUserAddresses, JmapError> {
    let list = result
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| JmapError::protocol("ParticipantIdentity/get result has no list"))?;
    Ok(CalendarUserAddresses::from_uris(
        list.iter().filter_map(participant_address),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_address_shapes_and_skips_an_identity_with_no_email() {
        let result = json!({ "list": [
            { "id": "a", "calendarAddress": "mailto:alice@example.org" },
            { "id": "b", "sendTo": { "imip": "mailto:a.smith@example.org" } },
            { "id": "c", "sendTo": { "web": "https://example.org/rsvp" } },
        ]});
        assert_eq!(
            from_get(&result).unwrap(),
            CalendarUserAddresses::Known(vec![
                "alice@example.org".to_owned(),
                "a.smith@example.org".to_owned(),
            ])
        );
    }

    #[test]
    fn a_result_without_a_list_is_a_protocol_error() {
        assert!(from_get(&json!({})).is_err());
    }
}
