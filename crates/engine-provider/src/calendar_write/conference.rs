//! Putting a video meeting on an event, and taking one off.
//!
//! A meeting reaches an event one of two ways, and an adapter writes them differently
//! (`calendar-semantics.md`, "Conferences"):
//!
//! - **The calendar's own server makes it** ([`NewConference::Native`]): Teams on an Exchange
//!   calendar, Meet on a Google one, for a service in
//!   [`Calendar::native_conferences`](engine_core::calendar::Calendar::native_conferences). The
//!   adapter asks; the link arrives with the write's answer or the next read.
//! - **It was made elsewhere** ([`NewConference::Attach`]): a link a person pasted, or one a
//!   meeting service or the host made. The adapter stores it in the protocol's meeting field where
//!   there is one, and in the notes and location where an invitee's calendar would otherwise not
//!   find it.

use engine_core::calendar::{Conference, ConferenceKey, ConferenceService, ConferenceText};
use serde::{Deserialize, Serialize};

use crate::{ProviderError, ProviderResult};

/// Refuses a write that carries a video meeting, for an adapter that writes none
/// ([`Capabilities::conference_writes`](crate::Capabilities::conference_writes)), so the event
/// is never stored without the meeting the caller asked for.
///
/// # Errors
///
/// [`ProviderError::invalid_state`] when `asked`.
pub fn refuse_conference(asked: bool) -> ProviderResult<()> {
    if asked {
        return Err(ProviderError::invalid_state(
            "this calendar's adapter writes no video meetings",
        ));
    }
    Ok(())
}

/// A meeting to put on an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NewConference {
    /// Ask the calendar's own server for a meeting on this service. The caller asks only for
    /// a service the calendar lists in `native_conferences`; an adapter refuses one its
    /// protocol cannot ask for.
    Native(ConferenceService),
    /// Store a meeting made elsewhere.
    ///
    /// Boxed, as [`RecurrenceEdit::Set`](crate::RecurrenceEdit::Set) is: unboxed, every
    /// patch would carry a whole conference and its words through the outbox.
    Attach(Box<AttachedConference>),
}

/// A meeting made elsewhere, and the words to describe it in the notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedConference {
    /// The meeting: its service, its ways in (a join link first), its name, ID and passcode.
    pub conference: Conference,
    /// The words of the block the notes carry for invitees whose calendar reads no meeting
    /// field.
    pub text: ConferenceText,
}

impl NewConference {
    /// A meeting made elsewhere, described in the notes with `text`.
    #[must_use]
    pub fn attach(conference: Conference, text: ConferenceText) -> Self {
        Self::Attach(Box::new(AttachedConference { conference, text }))
    }
}

/// What an edit does to the event's meetings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConferenceEdit {
    /// Put `new` on the event, taking the meeting named `replacing` off first.
    Set {
        /// The meeting `new` replaces, if any.
        replacing: Option<ConferenceKey>,
        /// The meeting to put on.
        new: NewConference,
    },
    /// Take the meeting with this key off the event, with the block of text and the
    /// location it left there.
    Remove(ConferenceKey),
}

impl ConferenceEdit {
    /// The meeting this edit takes off the event, if any.
    #[must_use]
    pub fn removes(&self) -> Option<&ConferenceKey> {
        match self {
            Self::Set { replacing, .. } => replacing.as_ref(),
            Self::Remove(key) => Some(key),
        }
    }

    /// The meeting this edit puts on the event, if any.
    #[must_use]
    pub fn adds(&self) -> Option<&NewConference> {
        match self {
            Self::Set { new, .. } => Some(new),
            Self::Remove(_) => None,
        }
    }
}
