//! Inbound scheduling (iTIP/iMIP).
//!
//! This models inbound scheduling (`calendar-semantics.md`) independent of
//! transport, in pure layers:
//!
//! - **Keys** — an instance is keyed by [`InstanceKey`] (`UID` +
//!   `RECURRENCE-ID`); among messages for one key the highest [`Revision`]
//!   (`SEQUENCE`, then `DTSTAMP`) wins (RFC 5546 §2.1.5), so a stale
//!   lower-`SEQUENCE` message never overrides a newer one.
//! - **Trust** — from the body, not the envelope (RFC 6047 §2.2.1/§2.3): a
//!   `REQUEST`/`CANCEL` is honored only when the authenticated sender matches the
//!   `ORGANIZER`, a `REPLY` only when it matches the replying `ATTENDEE`; an
//!   unauthenticated or mismatched message is never auto-applied
//!   ([`evaluate_imip_trust`]).
//! - **Message** — the normalized, parsed iTIP message ([`SchedulingMessage`]):
//!   the [`ScheduleMethod`] plus the carried event, from which the key, revision,
//!   and trust identities are derived. Producing it from a `text/calendar` body is
//!   the iCalendar parser's job (`provider-caldav`); this crate owns the shape.
//! - **Reconcile** — the decision ([`ScheduleAction`] from [`reconcile`]) and the
//!   pure application of it to a stored [`Event`](crate::calendar::Event): trust
//!   gate → supersession → `METHOD` dispatch (create/update, set `PARTSTAT`
//!   via [`apply_reply`], [`cancel`], or classify-and-surface the staged methods).
//! - **Detect** — the mail↔calendar bridge entry point: finding the iMIP
//!   `text/calendar` part in a message's MIME tree ([`find_calendar_part`]).
//!
//! Detecting the part on the mail path, fetching its bytes, parsing it, and
//! delivering an iTIP reply are the transport layers' jobs; this module fixes the
//! keys, ordering, trust decision, and the pure event mutation.

mod detect;
mod key;
mod message;
mod reconcile;
mod trust;

pub use detect::find_calendar_part;
pub use key::{InstanceKey, Revision};
pub use message::SchedulingMessage;
pub use reconcile::{ScheduleAction, apply_reply, cancel, reconcile};
pub use trust::{ImipTrust, ImipUntrusted, evaluate_imip_trust};

use serde::{Deserialize, Serialize};

open_enum! {
    /// An iTIP scheduling method (RFC 5546 §1.4). Canonical spelling is
    /// lowercase, matching JSCalendar `method`; the iMIP `method=` parameter and
    /// the iCalendar `METHOD` property are case-insensitive.
    ScheduleMethod {
        /// Informational, non-interactive copy; no reply expected.
        Publish => "publish",
        /// Create or update a scheduled object; a reply is expected.
        Request => "request",
        /// An attendee conveys their participation status to the organizer.
        Reply => "reply",
        /// Add instances to an existing recurring object.
        Add => "add",
        /// Cancel the object or specific instances.
        Cancel => "cancel",
        /// An attendee asks for the latest version.
        Refresh => "refresh",
        /// An attendee proposes a change.
        Counter => "counter",
        /// The organizer rejects a counter-proposal.
        DeclineCounter => "declinecounter",
    }
}

impl ScheduleMethod {
    /// Returns `true` if this method originates from the organizer (so trust is
    /// checked against the `ORGANIZER`). The complementary methods originate
    /// from an attendee.
    #[must_use]
    pub fn is_organizer_originated(&self) -> bool {
        matches!(
            self,
            Self::Publish | Self::Request | Self::Add | Self::Cancel | Self::DeclineCounter
        )
    }
}

/// Whether server-side scheduling or client-parsed iMIP applies to an account
/// (`calendar-semantics.md`).
///
/// CalDAV auto-schedule (RFC 6638) and JMAP `isOrigin`/scheduling are two
/// encodings of [`SchedulingMode::ServerAutoSchedule`]; pure IMAP/SMTP is
/// [`SchedulingMode::ClientImip`]. Callers query this rather than switching on
/// provider kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SchedulingMode {
    /// The provider runs iTIP server-side; the client reads the result.
    ServerAutoSchedule,
    /// The client parses inbound iMIP and sends iMIP replies itself.
    ClientImip,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_origin_classification() {
        assert!(ScheduleMethod::Request.is_organizer_originated());
        assert!(ScheduleMethod::Cancel.is_organizer_originated());
        assert!(!ScheduleMethod::Reply.is_organizer_originated());
        assert!(!ScheduleMethod::Counter.is_organizer_originated());
    }

    #[test]
    fn method_wire_strings_are_case_canonical() {
        assert_eq!(ScheduleMethod::Request.as_str(), "request");
        assert_eq!(ScheduleMethod::from_wire("reply"), ScheduleMethod::Reply);
        // An unknown method is preserved verbatim (open enum).
        assert_eq!(
            ScheduleMethod::from_wire("x-vendor"),
            ScheduleMethod::Other("x-vendor".into())
        );
    }

    #[test]
    fn scheduling_mode_roundtrips() {
        for mode in [
            SchedulingMode::ServerAutoSchedule,
            SchedulingMode::ClientImip,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<SchedulingMode>(&json).unwrap(), mode);
        }
    }
}
