//! Limits from the JMAP session resource: the core ones, and calendar's per-event cap.

use serde_json::Value;

use crate::{
    request::capability,
    session::{CoreLimits, Session},
};

pub(crate) fn parse_limits(core: &Value) -> CoreLimits {
    let defaults = CoreLimits::default();
    let read = |name: &str, fallback: usize| {
        core.get(name)
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|&value| value > 0)
            .unwrap_or(fallback)
    };
    CoreLimits {
        max_objects_in_get: read("maxObjectsInGet", defaults.max_objects_in_get),
        max_objects_in_set: read("maxObjectsInSet", defaults.max_objects_in_set),
        max_calls_in_request: read("maxCallsInRequest", defaults.max_calls_in_request),
        max_concurrent_requests: read("maxConcurrentRequests", defaults.max_concurrent_requests),
    }
}

pub(crate) fn max_participants_per_event(
    session: &Value,
    calendar_account_id: Option<&str>,
) -> Option<usize> {
    session
        .get("accounts")?
        .get(calendar_account_id?)?
        .get("accountCapabilities")?
        .get(capability::CALENDARS)?
        .get("maxParticipantsPerEvent")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

impl Session {
    pub(crate) fn max_participants_per_event(&self) -> Option<usize> {
        self.max_participants_per_event
    }
}
