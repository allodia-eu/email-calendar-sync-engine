//! `ParticipantIdentity/get`: the addresses the calendar account takes part in events as.
//!
//! JMAP Calendars keeps these as `ParticipantIdentity` objects on the calendars account, each
//! carrying the address the server schedules as for this user. They are the JMAP
//! counterpart of a CalDAV principal's `calendar-user-address-set`. An identity is shaped
//! like a JSCalendar participant, so its address is read the way a participant's is
//! ([`participant_address`]): `calendarAddress`, or the `sendTo` map of an earlier draft.

use engine_core::scheduling::addresses_match;
use engine_provider::CalendarUserAddresses;
use serde_json::{Value, json};

use crate::{
    calendar::participant_address,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
    session_accounts::SessionAccount,
    shared::looks_like_address,
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

/// The calendar user of a shared account, from the user's identities (`answer`).
///
/// `ParticipantIdentity/get` answers with the **user's** identities whichever account it is
/// asked on (pinned in `stalwart-harness/tests/smoke_shared_stores.rs`), so it never says who a
/// shared store's owner is. The owner is the account's `name` when that is an address (RFC
/// 8620 §1.6.2: "usually the primary email address"); the answer keeps the identities that are
/// the owner's. None of them is when the user may not schedule as the owner — a calendar
/// shared read-only — and that is [`CalendarUserAddresses::NotEnabled`]: in this store the
/// user schedules as nobody — unless the answer was partial (`Including`), which cannot rule the
/// owner out and so is `Unknown`. A store whose name is no address cannot be answered for.
pub(crate) fn for_store(
    answer: CalendarUserAddresses,
    store: &SessionAccount,
) -> CalendarUserAddresses {
    let Some(owner) = store
        .name
        .as_deref()
        .filter(|name| looks_like_address(name))
    else {
        return CalendarUserAddresses::Unknown;
    };
    let the_owners: Vec<String> = answer
        .addresses()
        .iter()
        .filter(|address| addresses_match(address, owner))
        .cloned()
        .collect();
    match answer {
        // A whole set without the owner rules the owner out; a partial one cannot.
        CalendarUserAddresses::Known(_) if the_owners.is_empty() => {
            CalendarUserAddresses::NotEnabled
        }
        CalendarUserAddresses::Including(_) if the_owners.is_empty() => {
            CalendarUserAddresses::Unknown
        }
        CalendarUserAddresses::Known(_) => CalendarUserAddresses::Known(the_owners),
        CalendarUserAddresses::Including(_) => CalendarUserAddresses::Including(the_owners),
        other => other,
    }
}

/// The addresses in a `ParticipantIdentity/get` result.
fn from_get(result: &Value) -> Result<CalendarUserAddresses, JmapError> {
    Ok(CalendarUserAddresses::from_uris(
        identities(result)?.iter().filter_map(participant_address),
    ))
}

fn identities(result: &Value) -> Result<&Vec<Value>, JmapError> {
    result
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| JmapError::protocol("ParticipantIdentity/get result has no list"))
}

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
            "properties": ["id", "calendarAddress", "sendTo", "isDefault"],
        }),
    );
    let response = executor.execute(&request).await?;
    identities(response.result(&call)?)?
        .iter()
        .find(|identity| {
            participant_address(identity).is_some_and(|address| addresses_match(address, organiser))
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
    fn a_partial_answer_without_the_owner_cannot_rule_the_owner_out() {
        let store = crate::session_accounts::parse_accounts(&json!({ "accounts": {
            "g": { "name": "support@example.org", "isPersonal": false }
        }}))
        .unwrap()
        .remove(0);
        // "These addresses, maybe more": the owner may be among the ones not named.
        let partial = CalendarUserAddresses::Including(vec!["alice@example.org".to_owned()]);
        assert_eq!(for_store(partial, &store), CalendarUserAddresses::Unknown);
        // A whole set that misses the owner does rule it out.
        let whole = CalendarUserAddresses::Known(vec!["alice@example.org".to_owned()]);
        assert_eq!(for_store(whole, &store), CalendarUserAddresses::NotEnabled);
    }

    #[test]
    fn a_result_without_a_list_is_a_protocol_error() {
        assert!(from_get(&json!({})).is_err());
    }
}
