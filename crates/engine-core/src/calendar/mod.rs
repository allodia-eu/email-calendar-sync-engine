//! The normalized calendar domain model (JSCalendar-shaped, RFC 8984).
//!
//! An [`Event`] is identified by a provider [`crate::ids::EventId`], carries the
//! cross-system [`crate::ids::Uid`], and belongs to a non-empty set of
//! [`Calendar`] collections. Time is the engine time model
//! ([`crate::time::CalendarDateTime`] + [`crate::time::Duration`], end =
//! start + duration). Recurrence is structural ([`RecurrenceRule`] +
//! [`Recurrence`] overrides/exclusions). Participants, locations, virtual
//! locations, and alerts complete the projection; provider-native payloads and
//! kind-specific data are preserved beside it.

mod alert;
mod calendar_collection;
mod event;
mod kind;
mod location;
mod participant;
mod recurrence;
mod recurrence_set;

pub use alert::{Alert, AlertAction, Trigger};
pub use calendar_collection::{Calendar, CalendarAccess};
pub use event::{Event, EventStatus, FreeBusyStatus, Privacy};
pub use kind::EventKind;
pub use location::{Location, RelativeTo, VirtualLocation};
pub use participant::{Participant, ParticipantKind, ParticipantRole, ParticipationStatus};
pub use recurrence::{Frequency, NDay, RecurrenceBound, RecurrenceRule, RecurrenceSkip, Weekday};
pub use recurrence_set::{Recurrence, RecurrenceOverride};
