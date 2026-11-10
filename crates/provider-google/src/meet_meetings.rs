//! Meet meetings made through the Meet REST API's `spaces.create`, for an event whose
//! calendar's server makes none of its own (`calendar-semantics.md`, "Meeting services").
//!
//! A space is the meeting: Meet names it, so the caller's title and times are not sent, and
//! it keeps no key of the caller's, so a repeated request makes a second space. The API has
//! no delete: a space nobody holds the link of is one nobody can join, and it is left.

use async_trait::async_trait;
use engine_core::calendar::{
    Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryPoint,
};
use engine_provider::{MeetingRequest, MeetingService, ProviderResult};
use serde_json::Value;

use crate::{error::GoogleError, json::opt_str, transport::GoogleClient};

/// What a Meet meeting attached to an event is called there.
const LABEL: &str = "Google Meet";

/// Meet meetings made as the user a Google token signed in as. The token needs
/// `https://www.googleapis.com/auth/meetings.space.created`.
#[derive(Debug)]
pub struct MeetMeetings {
    client: GoogleClient,
}

impl MeetMeetings {
    /// Meetings made through `client`.
    #[must_use]
    pub fn new(client: GoogleClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MeetingService for MeetMeetings {
    fn service(&self) -> ConferenceService {
        ConferenceService::GoogleMeet
    }

    async fn create_meeting(&self, _request: &MeetingRequest) -> ProviderResult<Conference> {
        let url = self.client.meet_url("/v2/spaces");
        let answer = self
            .client
            .post(&url, "application/json", b"{}".to_vec())
            .await?
            .ok_or_else(|| GoogleError::protocol("spaces.create answered without a space"))?;
        Ok(conference(&answer)?)
    }

    async fn delete_meeting(&self, _meeting: &Conference) -> ProviderResult<()> {
        Ok(())
    }
}

/// The space `spaces.create` answered with, as an event holds a meeting made elsewhere: the
/// join link, then each dial-in number with its PIN.
fn conference(space: &Value) -> Result<Conference, GoogleError> {
    let join = opt_str(space, "meetingUri")
        .and_then(|uri| ConferenceUri::parse(uri).ok())
        .ok_or_else(|| GoogleError::protocol("a space without a meeting link"))?;
    let mut entry_points = vec![EntryPoint::at(join.clone())];
    let numbers = space.get("phoneAccess").and_then(Value::as_array);
    for access in numbers.into_iter().flatten() {
        if let Some(entry) = phone(access)
            && !entry_points.iter().any(|known| known.uri == entry.uri)
        {
            entry_points.push(entry);
        }
    }
    Ok(Conference {
        key: ConferenceKey::new(join.as_str()),
        service: Some(ConferenceService::GoogleMeet),
        origin: ConferenceOrigin::Attached,
        entry_points,
        label: Some(LABEL.to_owned()),
        meeting_id: opt_str(space, "meetingCode")
            .filter(|code| !code.is_empty())
            .map(str::to_owned),
        notes: None,
    })
}

/// One `phoneAccess[]` entry as a `tel:` way in, or `None` without a number. Meet writes the
/// number in E.164, which is what a `tel:` URI dials.
fn phone(access: &Value) -> Option<EntryPoint> {
    let number = opt_str(access, "phoneNumber")?.trim();
    let mut entry = EntryPoint::at(ConferenceUri::parse(&format!("tel:{number}")).ok()?);
    entry.label = Some(number.to_owned());
    entry.access.pin = opt_str(access, "pin")
        .filter(|pin| !pin.is_empty())
        .map(str::to_owned);
    Some(entry)
}

#[cfg(test)]
#[path = "meet_meetings_tests.rs"]
mod tests;
