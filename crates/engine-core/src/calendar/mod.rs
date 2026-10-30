//! The normalized calendar domain model (JSCalendar-shaped, RFC 8984).
//!
//! An [`Event`] is identified by a provider [`crate::ids::EventId`], carries the
//! cross-system [`crate::ids::Uid`], and belongs to a non-empty set of
//! [`Calendar`] collections. Time is the engine time model
//! ([`crate::time::CalendarDateTime`] + [`crate::time::Duration`], end =
//! start + duration). Recurrence is structural ([`RecurrenceRule`] +
//! [`Recurrence`] overrides/exclusions). Participants, locations,
//! [`Conference`]s, and alerts complete the projection; provider-native payloads
//! and kind-specific data are preserved beside it.

mod alert;
mod calendar_collection;
mod conference;
mod conference_service;
mod conference_uri;
mod event;
mod kind;
mod location;
mod override_build;
mod participant;
mod recurrence;
mod recurrence_format;
mod recurrence_parse;
mod recurrence_set;

pub use alert::{Alert, AlertAction, Trigger};
pub use calendar_collection::{Calendar, CalendarAccess};
pub use conference::{
    AccessCodes, Conference, ConferenceFeature, ConferenceKey, ConferenceOrigin, EntryKind,
    EntryPoint, FlatEntry, NativeState, group_flat,
};
pub use conference_service::ConferenceService;
pub use conference_uri::{ConferenceUri, ConferenceUriError, UriScheme};
pub use event::{Event, EventStatus, FreeBusyStatus, Privacy};
pub use kind::EventKind;
pub use location::{Location, RelativeTo};
pub use override_build::OverrideBuilder;
pub use participant::{Participant, ParticipantKind, ParticipantRole, ParticipationStatus};
pub use recurrence::{Frequency, NDay, RecurrenceBound, RecurrenceRule, RecurrenceSkip, Weekday};
pub use recurrence_format::{RruleFormatError, UntilForm, format_rrule};
pub use recurrence_parse::{RruleParseError, parse_rrule};
pub use recurrence_set::{Recurrence, RecurrenceOverride};
