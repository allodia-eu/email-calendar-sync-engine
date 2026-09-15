//! Calendar limits from the JMAP session resource.

use serde_json::Value;

use crate::{request::capability, session::Session};

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
