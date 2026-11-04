//! Jitsi Meet, which has no API: a meeting is a room, and a room exists once someone opens its
//! link, `https://<server>/<room>`. So making one sends nothing, and the room's name is the
//! caller's [`MeetingKey`](super::MeetingKey), whose randomness is what keeps strangers out.

use async_trait::async_trait;
use engine_core::calendar::{
    Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryPoint,
    UriScheme,
};

use super::{MeetingRequest, MeetingService};
use crate::ProviderResult;

/// Meetings on one Jitsi server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitsiMeetings {
    /// The server's address, without a trailing `/`.
    server: String,
}

/// Why a Jitsi server's address was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitsiServerError;

impl core::fmt::Display for JitsiServerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a Jitsi server is an https address without a query or fragment")
    }
}

impl std::error::Error for JitsiServerError {}

impl JitsiMeetings {
    /// Meetings on the server at `server`, such as `https://meet.example.org`. A path is kept,
    /// for a server that serves Jitsi beneath one.
    ///
    /// # Errors
    ///
    /// [`JitsiServerError`] for anything but an `https` address, or one with a query or
    /// fragment, which a room name appended to it would not follow.
    pub fn new(server: &str) -> Result<Self, JitsiServerError> {
        let uri = ConferenceUri::parse(server).map_err(|_| JitsiServerError)?;
        if uri.scheme() != UriScheme::Https || uri.as_str().contains(['?', '#']) {
            return Err(JitsiServerError);
        }
        Ok(Self {
            server: uri.as_str().trim_end_matches('/').to_owned(),
        })
    }

    /// The server's address.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }
}

#[async_trait]
impl MeetingService for JitsiMeetings {
    fn service(&self) -> ConferenceService {
        ConferenceService::Jitsi
    }

    async fn create_meeting(&self, request: &MeetingRequest) -> ProviderResult<Conference> {
        let link = format!("{}/{}", self.server, request.key.as_str());
        let uri =
            ConferenceUri::parse(&link).expect("an https server and a lowercase key are a link");
        Ok(Conference {
            key: ConferenceKey::new(uri.as_str()),
            service: Some(ConferenceService::Jitsi),
            origin: ConferenceOrigin::Attached,
            entry_points: vec![EntryPoint::at(uri)],
            label: Some("Jitsi Meet".to_owned()),
            meeting_id: None,
            notes: None,
        })
    }

    /// A Jitsi room is held by no one once it is empty, so there is nothing to delete.
    async fn delete_meeting(&self, _meeting: &Conference) -> ProviderResult<()> {
        Ok(())
    }
}
