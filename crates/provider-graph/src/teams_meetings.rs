//! Teams meetings made through Graph's `onlineMeetings`, for an event whose calendar's server
//! makes none of its own (`calendar-semantics.md`, "Meeting services").
//!
//! `createOrGet` keeps the caller's key as the meeting's `externalId`, so a repeated request
//! answers with the meeting the first one made; after a delete it makes a new one. A meeting
//! is found again by its join link (`$filter=JoinWebUrl eq '…'`), which is what an event
//! holds, and Graph answers 404 for one that is gone, to the lookup and the delete alike.

use async_trait::async_trait;
use engine_core::calendar::{
    Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryPoint,
};
use engine_provider::{MeetingRequest, MeetingService, ProviderError, ProviderResult};
use serde_json::{Value, json};

use crate::{
    cal_conference::{dial_in_numbers, phone},
    categories::{encode, literal},
    error::GraphError,
    json::opt_str,
    mailbox_write::is_not_found,
    principal::encode_segment,
    transport::GraphClient,
};

/// What a Teams meeting attached to an event is called there.
const LABEL: &str = "Microsoft Teams";

/// Teams meetings organised by the user a Graph token signed in as.
///
/// Graph makes an online meeting only for the signed-in user (`/me`), so a client rooted at a
/// shared mailbox still makes the meetings as its own user. The token needs
/// `OnlineMeetings.ReadWrite`, which only a work or school account can grant.
#[derive(Debug)]
pub struct TeamsMeetings {
    client: GraphClient,
}

impl TeamsMeetings {
    /// Meetings made through `client`.
    #[must_use]
    pub fn new(client: GraphClient) -> Self {
        Self { client }
    }

    /// The ids of the meetings whose join link is `join`; none when Graph knows none.
    async fn find(&self, join: &ConferenceUri) -> Result<Vec<String>, GraphError> {
        let filter = format!("JoinWebUrl eq {}", literal(join.as_str()));
        let url = self
            .client
            .global_url(&format!("/me/onlineMeetings?$filter={}", encode(&filter)));
        let page = match self.client.get(&url).await {
            Ok(page) => page,
            Err(err) if is_not_found(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        Ok(page
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|meeting| opt_str(meeting, "id"))
            .map(str::to_owned)
            .collect())
    }
}

#[async_trait]
impl MeetingService for TeamsMeetings {
    fn service(&self) -> ConferenceService {
        ConferenceService::Teams
    }

    async fn create_meeting(&self, request: &MeetingRequest) -> ProviderResult<Conference> {
        let body = json!({
            "externalId": request.key.as_str(),
            "subject": request.title,
            "startDateTime": request.start.to_string(),
            "endDateTime": request.end.to_string(),
        });
        let url = self.client.global_url("/me/onlineMeetings/createOrGet");
        let answer = self
            .client
            .post(&url, "application/json", body.to_string().into_bytes())
            .await?
            .ok_or_else(|| GraphError::protocol("createOrGet answered without a meeting"))?;
        Ok(conference(&answer)?)
    }

    async fn delete_meeting(&self, meeting: &Conference) -> ProviderResult<()> {
        let join = meeting.join().ok_or_else(|| {
            ProviderError::invalid_state("a Teams meeting is found by its join link")
        })?;
        for id in self.find(join).await? {
            let url = self
                .client
                .global_url(&format!("/me/onlineMeetings/{}", encode_segment(&id)));
            match self.client.delete(&url, None).await {
                Ok(()) => {}
                Err(err) if is_not_found(&err) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }
}

/// The meeting `createOrGet` answered with, as an event holds one made elsewhere: the join
/// link with its passcode, then each dial-in number with the phone conference ID a caller
/// enters after dialling. That ID is `audioConferencing`'s own, not the join ID beside the
/// passcode.
fn conference(meeting: &Value) -> Result<Conference, GraphError> {
    let join = opt_str(meeting, "joinWebUrl")
        .and_then(|url| ConferenceUri::parse(url).ok())
        .ok_or_else(|| GraphError::protocol("a meeting without a join link"))?;
    let settings = meeting.get("joinMeetingIdSettings");
    let mut entry = EntryPoint::at(join.clone());
    entry.access.passcode = settings
        .and_then(|s| opt_str(s, "passcode"))
        .filter(|code| !code.is_empty())
        .map(str::to_owned);
    let mut entry_points = vec![entry];
    let audio = meeting.get("audioConferencing");
    let phone_id = audio
        .and_then(|a| opt_str(a, "conferenceId"))
        .filter(|id| !id.is_empty());
    for number in audio.map(dial_in_numbers).unwrap_or_default() {
        if let Some(mut entry) = phone(number)
            && !entry_points.iter().any(|known| known.uri == entry.uri)
        {
            entry.access.access_code = phone_id.map(str::to_owned);
            entry_points.push(entry);
        }
    }
    Ok(Conference {
        key: ConferenceKey::new(join.as_str()),
        service: Some(ConferenceService::Teams),
        origin: ConferenceOrigin::Attached,
        entry_points,
        label: Some(LABEL.to_owned()),
        meeting_id: settings
            .and_then(|s| opt_str(s, "joinMeetingId"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        notes: None,
    })
}

#[cfg(test)]
#[path = "teams_meetings_tests.rs"]
mod tests;
