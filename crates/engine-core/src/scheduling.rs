//! Inbound scheduling (iTIP/iMIP).
//!
//! This models the two load-bearing invariants of inbound scheduling
//! (`calendar-semantics.md`), independent of transport:
//!
//! - **Reconciliation by `(UID, SEQUENCE, RECURRENCE-ID)`** (RFC 5546 §2.1.5):
//!   an instance is keyed by [`InstanceKey`]; among messages for one key, the
//!   one with the highest [`Revision`] (`SEQUENCE`, then `DTSTAMP`) wins, so a
//!   stale lower-`SEQUENCE` message never overrides a newer one.
//! - **Trust from the body, not the envelope** (RFC 6047 §2.2.1/§2.3): a
//!   `REQUEST`/`CANCEL` is only honored when the authenticated sender matches the
//!   `ORGANIZER`, a `REPLY` only when it matches the replying `ATTENDEE`. An
//!   unauthenticated or mismatched message is never auto-applied. See
//!   [`evaluate_imip_trust`].
//!
//! Applying the result to stored events (creating, updating PARTSTAT, cancelling)
//! is the calendar layer's job; this module fixes the keys, ordering, and trust
//! decision.

use serde::{Deserialize, Serialize};

use crate::ids::Uid;
use crate::time::{CalendarDateTime, UtcDateTime};

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

/// Identifies a single scheduling target: a whole series, or one instance of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstanceKey {
    /// The event's cross-system uid.
    pub uid: Uid,
    /// The recurrence id of a single targeted instance, or `None` for the whole
    /// series/master.
    pub recurrence_id: Option<CalendarDateTime>,
}

impl InstanceKey {
    /// A key targeting the whole series (no recurrence id).
    #[must_use]
    pub fn series(uid: Uid) -> Self {
        Self {
            uid,
            recurrence_id: None,
        }
    }

    /// A key targeting a single instance.
    #[must_use]
    pub fn instance(uid: Uid, recurrence_id: CalendarDateTime) -> Self {
        Self {
            uid,
            recurrence_id: Some(recurrence_id),
        }
    }

    /// Returns `true` if this key targets the whole series rather than one
    /// instance.
    #[must_use]
    pub fn is_series(&self) -> bool {
        self.recurrence_id.is_none()
    }
}

/// The revision of a scheduling message for one [`InstanceKey`]: its `SEQUENCE`
/// and `DTSTAMP`.
///
/// `Ord` compares `sequence` first, then `dtstamp`, so the maximum revision is
/// the winner of iTIP message sequencing (RFC 5546 §2.1.5). The field order is
/// load-bearing for the derived ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Revision {
    /// The iTIP `SEQUENCE` (higher obsoletes lower).
    pub sequence: u32,
    /// The `DTSTAMP`, the tie-breaker when sequences are equal.
    pub dtstamp: UtcDateTime,
}

impl Revision {
    /// Creates a revision.
    #[must_use]
    pub fn new(sequence: u32, dtstamp: UtcDateTime) -> Self {
        Self { sequence, dtstamp }
    }

    /// Returns `true` if `self` supersedes `current` — a strictly higher
    /// `SEQUENCE`, or an equal `SEQUENCE` with a later `DTSTAMP`. An equal
    /// revision does **not** supersede (idempotent re-delivery is ignored).
    #[must_use]
    pub fn supersedes(&self, current: &Revision) -> bool {
        self > current
    }
}

/// The reason a scheduling message is not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImipUntrusted {
    /// The message carried no authenticated sender (e.g. unsigned), so nothing
    /// can be verified.
    #[error("scheduling message has no authenticated sender")]
    Unauthenticated,
    /// The relevant body identity (organizer or attendee) was absent.
    #[error("scheduling message is missing the identity to verify against")]
    MissingIdentity,
    /// The authenticated sender did not match the expected body identity.
    #[error("authenticated sender does not match the {expected} in the message body")]
    SenderMismatch {
        /// Which body identity was expected (`organizer` or `attendee`).
        expected: &'static str,
    },
}

/// The trust verdict for an inbound scheduling message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImipTrust {
    /// Safe to apply: the authenticated sender matches the body identity.
    Trusted,
    /// Must not be auto-applied; carries the reason.
    Untrusted(ImipUntrusted),
}

/// Normalizes a calendar address for comparison: lowercased, with a leading
/// `mailto:` scheme removed.
fn normalize_address(address: &str) -> String {
    let lowered = address.trim().to_ascii_lowercase();
    lowered
        .strip_prefix("mailto:")
        .unwrap_or(&lowered)
        .to_owned()
}

/// Decides whether an inbound iMIP message may be auto-applied.
///
/// Trust is verified against the **body** identity (`ORGANIZER`/`ATTENDEE`),
/// never the email envelope `From` (RFC 6047 §2.3): an organizer-originated
/// method must match the `organizer`, an attendee-originated one the
/// `replying_attendee`. A message with no `authenticated_sender` is always
/// untrusted (unsigned messages may not be trusted, RFC 6047 §2.2.1).
#[must_use]
pub fn evaluate_imip_trust(
    method: &ScheduleMethod,
    organizer: Option<&str>,
    replying_attendee: Option<&str>,
    authenticated_sender: Option<&str>,
) -> ImipTrust {
    let Some(sender) = authenticated_sender else {
        return ImipTrust::Untrusted(ImipUntrusted::Unauthenticated);
    };
    let (expected, label) = if method.is_organizer_originated() {
        (organizer, "organizer")
    } else {
        (replying_attendee, "attendee")
    };
    match expected {
        None => ImipTrust::Untrusted(ImipUntrusted::MissingIdentity),
        Some(identity) if normalize_address(identity) == normalize_address(sender) => {
            ImipTrust::Trusted
        }
        Some(_) => ImipTrust::Untrusted(ImipUntrusted::SenderMismatch { expected: label }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dtstamp(s: &str) -> UtcDateTime {
        s.parse().unwrap()
    }

    #[test]
    fn higher_sequence_supersedes() {
        let v1 = Revision::new(1, dtstamp("2021-01-01T00:00:00Z"));
        let v2 = Revision::new(2, dtstamp("2020-01-01T00:00:00Z")); // older stamp, higher seq
        assert!(v2.supersedes(&v1));
        assert!(!v1.supersedes(&v2)); // a stale lower-SEQUENCE message loses
    }

    #[test]
    fn equal_sequence_breaks_on_dtstamp() {
        let earlier = Revision::new(3, dtstamp("2021-01-01T09:00:00Z"));
        let later = Revision::new(3, dtstamp("2021-01-01T10:00:00Z"));
        assert!(later.supersedes(&earlier));
        // Idempotent re-delivery (identical revision) does not supersede.
        assert!(!later.supersedes(&later.clone()));
    }

    #[test]
    fn instance_key_distinguishes_series_from_instance() {
        let uid = Uid::new("uid-1").unwrap();
        let series = InstanceKey::series(uid.clone());
        let instance = InstanceKey::instance(
            uid,
            CalendarDateTime::Floating("2021-06-07T09:00:00".parse().unwrap()),
        );
        assert!(series.is_series());
        assert!(!instance.is_series());
        assert_ne!(series, instance);
    }

    #[test]
    fn request_trusts_only_matching_organizer() {
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Request,
            Some("mailto:boss@example.com"),
            None,
            Some("BOSS@example.com"), // case- and scheme-insensitive match
        );
        assert_eq!(trust, ImipTrust::Trusted);

        let spoofed = evaluate_imip_trust(
            &ScheduleMethod::Cancel,
            Some("boss@example.com"),
            None,
            Some("attacker@evil.example"),
        );
        assert_eq!(
            spoofed,
            ImipTrust::Untrusted(ImipUntrusted::SenderMismatch {
                expected: "organizer"
            })
        );
    }

    #[test]
    fn reply_trusts_only_matching_attendee() {
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Reply,
            Some("boss@example.com"),
            Some("guest@example.com"),
            Some("guest@example.com"),
        );
        assert_eq!(trust, ImipTrust::Trusted);

        // A reply that authenticates as the organizer (not the attendee) is not
        // trusted to set the attendee's status.
        let mismatch = evaluate_imip_trust(
            &ScheduleMethod::Reply,
            Some("boss@example.com"),
            Some("guest@example.com"),
            Some("boss@example.com"),
        );
        assert_eq!(
            mismatch,
            ImipTrust::Untrusted(ImipUntrusted::SenderMismatch {
                expected: "attendee"
            })
        );
    }

    #[test]
    fn unauthenticated_message_is_never_trusted() {
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Request,
            Some("boss@example.com"),
            None,
            None,
        );
        assert_eq!(trust, ImipTrust::Untrusted(ImipUntrusted::Unauthenticated));
    }

    #[test]
    fn method_origin_classification() {
        assert!(ScheduleMethod::Request.is_organizer_originated());
        assert!(ScheduleMethod::Cancel.is_organizer_originated());
        assert!(!ScheduleMethod::Reply.is_organizer_originated());
        assert!(!ScheduleMethod::Counter.is_organizer_originated());
    }
}
