//! Making a video meeting at a meeting service, for an event whose calendar's server makes
//! none of its own (`calendar-semantics.md`, "Meeting services").
//!
//! A service makes the meeting and answers with its ways in; the caller then puts it on the
//! event as one made elsewhere ([`NewConference::attach`](crate::NewConference::attach)). The
//! service is told only what [`MeetingRequest`] holds: no invitees, no notes, no location.

mod jitsi;

use async_trait::async_trait;
use engine_core::{
    calendar::{Conference, ConferenceService},
    time::UtcDateTime,
};
pub use jitsi::{JitsiMeetings, JitsiServerError};
use serde::{Deserialize, Serialize};

use crate::ProviderResult;

/// A service that makes video meetings.
#[async_trait]
pub trait MeetingService: Send + Sync {
    /// The service the meetings run on.
    fn service(&self) -> ConferenceService;

    /// Makes a meeting for `request`. Where the service keeps a key (Teams' `externalId`),
    /// the same [`MeetingKey`] answers with the same meeting, so a retry makes no second one.
    ///
    /// # Errors
    ///
    /// The service's refusal or a transport failure, classified as any provider error is.
    async fn create_meeting(&self, request: &MeetingRequest) -> ProviderResult<Conference>;

    /// Deletes a meeting this service made, named by a conference as an event holds it: the
    /// one [`create_meeting`](Self::create_meeting) answered with, or the same meeting read back
    /// from the event. The service finds its meeting by the join link, so nothing of the
    /// service's own needs keeping beside the event. A meeting already gone is a success.
    ///
    /// # Errors
    ///
    /// The service's refusal or a transport failure.
    async fn delete_meeting(&self, meeting: &Conference) -> ProviderResult<()>;
}

/// Everything a service is told about the meeting it makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingRequest {
    /// Names the meeting to the service, and is the room on a service whose link is the name.
    pub key: MeetingKey,
    /// What a service that names its meetings calls this one: the event's title, or the
    /// caller's word for a meeting when it has none.
    pub title: String,
    /// When the meeting starts.
    pub start: UtcDateTime,
    /// When it ends; for a series, when its last occurrence ends, since a service that expires
    /// a meeting does so after its end. A series without one has no such end, and the caller
    /// names how long the meeting is to last instead.
    pub end: UtcDateTime,
}

/// A meeting's name at its service, minted by the caller from at least 128 bits of a
/// cryptographically secure random source: anyone who has it can join a meeting whose link is
/// the name. 26 to 64 lowercase letters and digits, such as 128 bits in lowercase base32;
/// Jitsi lowercases a room name, so an uppercase letter would name a different key from the
/// room it opens. Its `Debug` hides it, as `Credentials` hide a password, because the key
/// alone is a way in.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MeetingKey(String);

impl core::fmt::Debug for MeetingKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("MeetingKey").field(&"<redacted>").finish()
    }
}

/// Why a [`MeetingKey`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingKeyError;

impl core::fmt::Display for MeetingKeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a meeting key is 26 to 64 lowercase letters and digits")
    }
}

impl std::error::Error for MeetingKeyError {}

impl MeetingKey {
    /// Wraps a caller-minted key.
    ///
    /// # Errors
    ///
    /// [`MeetingKeyError`] for a key too short to carry 128 bits, longer than 64, or holding
    /// anything but lowercase ASCII letters and digits.
    pub fn new(key: impl Into<String>) -> Result<Self, MeetingKeyError> {
        let key = key.into();
        let shaped = (26..=64).contains(&key.len())
            && key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if shaped {
            Ok(Self(key))
        } else {
            Err(MeetingKeyError)
        }
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MeetingKey {
    type Error = MeetingKeyError;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        Self::new(key)
    }
}

impl From<MeetingKey> for String {
    fn from(key: MeetingKey) -> Self {
        key.0
    }
}

#[cfg(test)]
#[path = "meeting_tests.rs"]
mod tests;
