//! Tests for reading `SCHEDULE-STATUS` off a stored scheduling object.
//!
//! The fixtures are **observed bytes**, not invented ones. Two real servers disagree about
//! this parameter in a way no spec reading predicts, and both shapes are pinned here:
//!
//! - **Soverin** (SabreDAV + `Schedule` plugin) writes `ORGANIZER;SCHEDULE-STATUS=5.2` on the
//!   attendee's copy and delivers nothing — 26 such meetings on one real account, and not one
//!   successful reply ever.
//! - **Stalwart** writes **no** `ORGANIZER;SCHEDULE-STATUS` at all, on success *or* with an
//!   unreachable organizer, while delivering the reply correctly (proven by the organizer's
//!   separate copy showing `PARTSTAT=ACCEPTED`).
//!
//! That pair is the whole argument for three states rather than two.

use super::*;

/// Soverin's real reply-failure shape, with the identifiers replaced by example ones. The
/// `ATTENDEE` line is long enough that the server folds it, which is why the fold is preserved
/// here rather than tidied away — the addresses are stand-ins, the *bytes around them* are not.
const SOVERIN_FAILED: &str = "BEGIN:VCALENDAR\r\n\
     VERSION:2.0\r\n\
     PRODID:-//Sabre//Sabre VObject 4.5.8//EN\r\n\
     BEGIN:VEVENT\r\n\
     UID:3f2b9c14-0e6a-4d18-9f77-5c1ab3e08d42\r\n\
     SUMMARY:Test invitation\r\n\
     ORGANIZER;SCHEDULE-STATUS=5.2:mailto:organizer@example.net\r\n\
     ATTENDEE;CN=attendee@example.org;RSVP=TRUE;PARTSTAT=ACCEPTED:mailto:attend\r\n\
     \x20ee@example.org\r\n\
     END:VEVENT\r\n\
     END:VCALENDAR\r\n";

/// Stalwart's real shape after a successful RSVP: the answer is stored, and the server says
/// nothing whatsoever about having delivered it.
const STALWART_SILENT: &str = "BEGIN:VCALENDAR\r\n\
     VERSION:2.0\r\n\
     PRODID:-//Example//probe//EN\r\n\
     BEGIN:VEVENT\r\n\
     UID:reply-delivery-probe-001@test.local\r\n\
     SUMMARY:Reply delivery probe\r\n\
     ORGANIZER:mailto:bob@test.local\r\n\
     ATTENDEE;RSVP=TRUE;PARTSTAT=ACCEPTED:mailto:carol@test.local\r\n\
     END:VEVENT\r\n\
     END:VCALENDAR\r\n";

#[test]
fn a_reported_permanent_failure_is_a_failure() {
    assert_eq!(
        reply_delivery(SOVERIN_FAILED),
        ReplyDelivery::Failed {
            status: "5.2".to_owned()
        }
    );
}

#[test]
fn a_server_that_says_nothing_reports_nothing_rather_than_success() {
    // The load-bearing case. Stalwart delivered this reply and wrote no status; reading that
    // silence as a delivery is indistinguishable from reading Soverin's silence — which does
    // not exist, because Soverin speaks. Neither may be guessed at.
    assert_eq!(reply_delivery(STALWART_SILENT), ReplyDelivery::NotReported);
}

#[test]
fn every_success_class_is_a_delivery() {
    for status in ["1.0", "1.1", "1.2", "2.0"] {
        let ical = format!(
            "BEGIN:VEVENT\r\nORGANIZER;SCHEDULE-STATUS={status}:mailto:a@example.com\r\nEND:VEVENT\r\n"
        );
        assert_eq!(
            reply_delivery(&ical),
            ReplyDelivery::Delivered {
                status: status.to_owned()
            },
            "{status}"
        );
    }
}

#[test]
fn every_failure_class_is_a_failure() {
    for status in ["3.7", "3.8", "5.1", "5.2", "5.3"] {
        let ical = format!(
            "BEGIN:VEVENT\r\nORGANIZER;SCHEDULE-STATUS={status}:mailto:a@example.com\r\nEND:VEVENT\r\n"
        );
        assert!(reply_delivery(&ical).failed(), "{status}");
    }
}

#[test]
fn an_undefined_status_class_keeps_its_token_rather_than_being_discarded() {
    // RFC 5546 §3.6 defines 1.x–5.x with no 4.x. A class we do not understand is not a
    // licence to pick the convenient answer — and it is precisely the value someone
    // debugging an unusual server needs to see, so it survives rather than becoming
    // "nothing was reported".
    let ical =
        "BEGIN:VEVENT\r\nORGANIZER;SCHEDULE-STATUS=4.0:mailto:a@example.com\r\nEND:VEVENT\r\n";
    let verdict = reply_delivery(ical);
    assert_eq!(
        verdict,
        ReplyDelivery::Unrecognized {
            status: "4.0".to_owned()
        }
    );
    assert!(!verdict.failed(), "an unknown class is not actionable");
    assert_eq!(verdict.status(), Some("4.0"), "the token is kept for a log");
}

#[test]
fn a_non_numeric_status_is_kept_rather_than_dropped() {
    let ical =
        "BEGIN:VEVENT\r\nORGANIZER;SCHEDULE-STATUS=weird:mailto:a@example.com\r\nEND:VEVENT\r\n";
    assert_eq!(reply_delivery(ical).status(), Some("weird"));
}

#[test]
fn a_status_on_an_attendee_is_a_different_message_and_is_ignored() {
    // ATTENDEE;SCHEDULE-STATUS is the REQUEST we sent *as organizer*. Reading it as ours
    // reports a delivered reply on a meeting the organizer was never told about. Both
    // property orders, because a fixture that happens to put ORGANIZER first passes even
    // against code that accepts either property.
    for organizer_first in [true, false] {
        let organizer = "ORGANIZER:mailto:boss@example.com";
        let attendee = "ATTENDEE;SCHEDULE-STATUS=1.1;PARTSTAT=ACCEPTED:mailto:me@example.com";
        let body = if organizer_first {
            format!("{organizer}\r\n{attendee}")
        } else {
            format!("{attendee}\r\n{organizer}")
        };
        let ical = format!("BEGIN:VEVENT\r\n{body}\r\nEND:VEVENT\r\n");
        assert_eq!(
            reply_delivery(&ical),
            ReplyDelivery::NotReported,
            "organizer_first={organizer_first}"
        );
    }
}

#[test]
fn a_status_split_across_a_fold_is_still_read() {
    // ORGANIZER with a status and a CN passes 75 octets, so the parameter routinely lands
    // mid-fold. Here the fold falls inside the parameter *name*, where no scan of physical
    // lines can see it — and the resulting "no status" reads as a clean success.
    let ical = "BEGIN:VEVENT\r\n\
         ORGANIZER;CN=An Organizer With A Padded Name;SCHEDULE-STAT\r\n\
         \x20US=5.2:mailto:organizer@example.net\r\n\
         END:VEVENT\r\n";
    assert_eq!(
        reply_delivery(ical),
        ReplyDelivery::Failed {
            status: "5.2".to_owned()
        }
    );
}

#[test]
fn a_parameter_name_is_matched_case_insensitively() {
    // RFC 5545 §3.1: parameter names are case-insensitive. Servers do vary.
    let ical =
        "BEGIN:VEVENT\r\nORGANIZER;schedule-status=5.2:mailto:a@example.com\r\nEND:VEVENT\r\n";
    assert!(reply_delivery(ical).failed());
}

#[test]
fn a_quoted_status_carrying_a_description_keeps_its_class() {
    let ical = "BEGIN:VEVENT\r\n\
         ORGANIZER;SCHEDULE-STATUS=\"5.2;Could not deliver\":mailto:a@example.com\r\n\
         END:VEVENT\r\n";
    assert_eq!(
        reply_delivery(ical),
        ReplyDelivery::Failed {
            status: "5.2;Could not deliver".to_owned()
        }
    );
}

#[test]
fn a_colon_inside_a_quoted_parameter_does_not_end_the_parameters() {
    let ical = "BEGIN:VEVENT\r\n\
         ORGANIZER;CN=\"Doe: Jane\";SCHEDULE-STATUS=5.2:mailto:a@example.com\r\n\
         END:VEVENT\r\n";
    assert!(reply_delivery(ical).failed());
}

#[test]
fn a_semicolon_inside_the_value_is_not_a_parameter() {
    // No parameters at all — everything after the first unquoted colon is the value.
    let ical = "BEGIN:VEVENT\r\nORGANIZER:mailto:weird;address@example.com\r\nEND:VEVENT\r\n";
    assert_eq!(reply_delivery(ical), ReplyDelivery::NotReported);
}

#[test]
fn a_document_with_no_organizer_reports_nothing() {
    let ical = "BEGIN:VEVENT\r\nSUMMARY:Just my own event\r\nEND:VEVENT\r\n";
    assert_eq!(reply_delivery(ical), ReplyDelivery::NotReported);
}

#[test]
fn an_empty_document_reports_nothing() {
    assert_eq!(reply_delivery(""), ReplyDelivery::NotReported);
}

#[test]
fn a_property_merely_starting_with_organizer_is_not_the_organizer() {
    // `X-ORGANIZER-HINT` is not ORGANIZER; a prefix match would read a status off it.
    let ical = "BEGIN:VEVENT\r\n\
         X-ORGANIZER-HINT;SCHEDULE-STATUS=5.2:mailto:a@example.com\r\n\
         END:VEVENT\r\n";
    assert_eq!(reply_delivery(ical), ReplyDelivery::NotReported);
}
