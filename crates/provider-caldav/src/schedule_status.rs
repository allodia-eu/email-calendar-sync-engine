//! Reading RFC 6638 §3.2.9 `SCHEDULE-STATUS` off a stored scheduling object — the server's
//! own report of what became of the iTIP message it sent on our behalf.
//!
//! # Which property carries it decides the direction
//!
//! The parameter lives on the property naming the calendar user the message was sent *to*, in
//! the sender's own copy. On an invitation **we** answered, that is the `ORGANIZER`:
//!
//! ```text
//! ORGANIZER;SCHEDULE-STATUS=5.2:mailto:boss@example.com   the REPLY we sent to the organizer
//! ATTENDEE;SCHEDULE-STATUS=1.1:mailto:guest@example.com   a REQUEST we sent as the organizer
//! ```
//!
//! So [`reply_delivery`] reads **only** the `ORGANIZER` line. Reading an `ATTENDEE` one would
//! report the delivery of a completely different message — and since a meeting we organized
//! routinely carries a *successful* `ATTENDEE` status, the mistake reports a delivered reply
//! on a meeting whose organizer was never told.
//!
//! # Absence is not success
//!
//! Most servers never write this parameter at all, including auto-scheduling ones that
//! deliver perfectly (Stalwart does not write it in either direction of an RSVP, verified
//! live). So an absent status is [`ReplyDelivery::NotReported`] — *no information* — and
//! never a delivery. Treating silence as success is the bug this module exists to prevent:
//! it renders a permanent, reported failure to the user as "You accepted".

use engine_ical::{Document, split_once_unquoted, split_unquoted};
use engine_provider::ReplyDelivery;

/// The `SCHEDULE-STATUS` parameter name (RFC 6638 §3.2.9).
const SCHEDULE_STATUS: &str = "SCHEDULE-STATUS";

/// What the server reported about delivering **our** reply to the organizer.
///
/// Returns [`ReplyDelivery::NotReported`] for a document with no `ORGANIZER` or no status on
/// it, and [`ReplyDelivery::Unrecognized`] for a status whose class RFC 5546 §3.6 does not
/// define — which keeps the token for a support log rather than guessing a verdict from it.
pub fn reply_delivery(raw_ical: &str) -> ReplyDelivery {
    let Some(status) = organizer_status(raw_ical) else {
        return ReplyDelivery::NotReported;
    };
    classify(&status)
}

/// The `SCHEDULE-STATUS` parameter of the first `ORGANIZER` property, if there is one.
///
/// Uses the shared logical-line model, so a status split across a fold — which is the common
/// case, since `ORGANIZER` with a status and a `CN` passes 75 octets — is read whole. Scanning
/// physical lines finds nothing there and reports a clean success.
fn organizer_status(raw_ical: &str) -> Option<String> {
    let doc = Document::parse(raw_ical);
    (0..doc.len())
        .map(|index| doc.logical(index))
        .find(|line| is_property(line, "ORGANIZER"))
        .and_then(|line| parameter(&line, SCHEDULE_STATUS))
}

/// Whether a logical line is the named property.
fn is_property(logical: &str, name: &str) -> bool {
    let end = logical.find([';', ':']).unwrap_or(logical.len());
    logical[..end].trim().eq_ignore_ascii_case(name)
}

/// The value of parameter `name` on a logical line, unquoted.
///
/// Parameters end at the first **unquoted** colon: a `CN` may hold one (`CN="Doe: J"`),
/// and a value may hold a semicolon (`ORGANIZER:mailto:a;b`, which has no parameters at all).
fn parameter(logical: &str, name: &str) -> Option<String> {
    let (head, _) = split_once_unquoted(logical, ':')?;
    let segments = split_unquoted(head, ';');
    segments.iter().skip(1).find_map(|segment| {
        let (key, value) = segment.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Maps an RFC 5546 §3.6 status to a verdict by its **class** — the digit before the first
/// dot. Classes 1 (pending/sent) and 2 (success) delivered; 3 (invalid request) and 5
/// (service failure) did not.
///
/// The status may carry a human description after a semicolon
/// (`"5.2;Could not deliver"`), and servers differ on the sub-code, so only the class is read.
fn classify(status: &str) -> ReplyDelivery {
    let owned = status.to_owned();
    match status.as_bytes().first() {
        Some(b'1' | b'2') => ReplyDelivery::Delivered { status: owned },
        Some(b'3' | b'5') => ReplyDelivery::Failed { status: owned },
        _ => ReplyDelivery::Unrecognized { status: owned },
    }
}

#[cfg(test)]
#[path = "schedule_status_tests.rs"]
mod tests;
