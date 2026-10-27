//! The engine's id for a Google event: `<calendar>/<event>`.
//!
//! A Google event id is unique within its calendar only. One meeting carries the same id in
//! every calendar it shows in, and every call that names an event names its calendar as well,
//! so the pair is the event's identity. A Google event id is base32hex, with `_` joining an
//! occurrence to its series, so the **last** `/` always separates the two halves.
//!
//! A calendar is named by the id its listing gave it. Google also answers to `primary` for the
//! default calendar, which the listing names by its address; the alias would be a second id for
//! one calendar, with events and a scope of its own, so it is refused.

use engine_core::ids::{CalendarId, EventId, ProviderKey};
use engine_provider::{ProviderError, ProviderResult};

use crate::{error::GoogleError, json::wrap_id};

/// Google's alias for the account's default calendar.
const PRIMARY_ALIAS: &str = "primary";

/// `calendar` as Google names it in a request.
///
/// # Errors
///
/// [`ProviderError::invalid_state`] for the `primary` alias.
pub(crate) fn listed(calendar: &CalendarId) -> ProviderResult<&str> {
    let id = calendar.key().as_str();
    if id == PRIMARY_ALIAS {
        return Err(ProviderError::invalid_state(
            "a Google calendar is named by its listed id, not the primary alias",
        ));
    }
    Ok(id)
}

/// The key of Google event `event` in `calendar`.
pub(crate) fn key(calendar: &str, event: &str) -> Result<ProviderKey, GoogleError> {
    wrap_id(ProviderKey::new(qualified(calendar, event)), "event id")
}

/// The [`EventId`] of Google event `event` in `calendar`.
pub(crate) fn event_id(calendar: &str, event: &str) -> Result<EventId, GoogleError> {
    wrap_id(
        EventId::try_from(qualified(calendar, event).as_str()),
        "event id",
    )
}

/// The calendar and the Google event id an [`EventId`] names.
///
/// # Errors
///
/// [`ProviderError::invalid_state`] for an id this adapter did not mint.
pub(crate) fn split(id: &EventId) -> ProviderResult<(&str, &str)> {
    id.key()
        .as_str()
        .rsplit_once('/')
        .filter(|(calendar, event)| {
            !calendar.is_empty() && *calendar != PRIMARY_ALIAS && !event.is_empty()
        })
        .ok_or_else(|| ProviderError::invalid_state("the event id names no Google calendar"))
}

fn qualified(calendar: &str, event: &str) -> String {
    format!("{calendar}/{event}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_carries_its_calendar_and_splits_back() {
        let calendar = "team@group.example.com";
        let id = event_id(calendar, "abc123_20260914T073000Z").unwrap();
        assert_eq!(
            id.key().as_str(),
            "team@group.example.com/abc123_20260914T073000Z"
        );
        assert_eq!(
            split(&id).unwrap(),
            ("team@group.example.com", "abc123_20260914T073000Z")
        );
        assert_eq!(
            key(calendar, "abc123").unwrap().as_str(),
            "team@group.example.com/abc123"
        );
    }

    #[test]
    fn one_google_id_in_two_calendars_is_two_events() {
        assert_ne!(
            event_id("me@example.com", "abc123").unwrap(),
            event_id("team@group.example.com", "abc123").unwrap()
        );
    }

    #[test]
    fn the_default_calendar_is_named_by_its_address_not_the_alias() {
        let address = CalendarId::try_from("me@example.com").unwrap();
        assert_eq!(listed(&address).unwrap(), "me@example.com");
        assert!(listed(&CalendarId::try_from("primary").unwrap()).is_err());
    }

    #[test]
    fn an_id_without_a_calendar_is_refused() {
        for bare in ["abc123", "/abc123", "me@example.com/", "primary/abc123"] {
            let id = EventId::try_from(bare).unwrap();
            assert!(split(&id).is_err(), "{bare}");
        }
    }
}
