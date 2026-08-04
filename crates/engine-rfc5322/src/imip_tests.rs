//! Tests for the iMIP assembly (RFC 6047): an iTIP object carried as an **alternative
//! body part**, driven through the full `assemble_message`.
//!
//! What makes these worth their own file rather than a case in `mime_tests.rs`: an iMIP
//! message is not "a message with a calendar file in it". Every assertion here is a rule
//! from RFC 6047 §2.4/§2.5 that, broken, produces a message a receiving client files as a
//! document instead of processing as a scheduling reply — and the sender sees no error.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress, scheduling::ScheduleMethod};
use engine_provider::{Draft, DraftAttachment, DraftCalendar};
use time::macros::datetime;

use super::*;

/// An iTIP `REPLY` object: the answer an attendee sends when their calendar server will
/// not send it for them (`Capabilities::calendar_scheduling` is false).
const REPLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nMETHOD:REPLY\r\n\
                     BEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\n\
                     ORGANIZER:mailto:boss@test.local\r\n\
                     ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@test.local\r\nSEQUENCE:0\r\n\
                     END:VEVENT\r\nEND:VCALENDAR\r\n";

fn reply_draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("imip@host").unwrap(),
        EmailAddress::new("me@test.local"),
        vec![EmailAddress::new("boss@test.local")],
        "Accepted: Sprint planning",
        "Accepted: Sprint planning",
    )
    .with_calendar(DraftCalendar::new(ScheduleMethod::Reply, REPLY))
}

fn assemble(draft: &Draft) -> String {
    String::from_utf8(assemble_message(draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap()).unwrap()
}

/// The body of the part whose headers contain `marker`, decoded from base64.
fn decoded_part(message: &str, marker: &str) -> String {
    let start = message.find(marker).expect("part present");
    let body = &message[start..];
    let body = &body[body.find("\r\n\r\n").expect("header/body break") + 4..];
    let encoded: String = body
        .lines()
        .take_while(|line| !line.starts_with("--"))
        .collect();
    String::from_utf8(base64_decode(&encoded)).expect("utf-8 body")
}

/// Minimal RFC 4648 decoder — the assembler only encodes, so the test owns the inverse.
fn base64_decode(input: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
    {
        let value = ALPHABET
            .iter()
            .position(|c| *c == byte)
            .expect("base64 alphabet");
        buffer = (buffer << 6) | u32::try_from(value).expect("6-bit value");
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("byte"));
        }
    }
    out
}

#[test]
fn the_itip_object_is_an_alternative_sibling_of_the_text_body() {
    // RFC 6047 §2.4: the scheduling object is a *representation of the message*, so it sits
    // beside the human-readable text inside `multipart/alternative` — not enclosed by it as
    // a `multipart/mixed` attachment would be. A client that understands `text/calendar`
    // takes the last alternative; one that does not shows the text.
    let message = assemble(&reply_draft());
    assert!(
        message.contains("Content-Type: multipart/alternative; boundary=\""),
        "{message}"
    );
    assert!(
        !message.contains("multipart/mixed"),
        "an iTIP-only draft needs no mixed wrapper: {message}"
    );

    let text = message.find("Content-Type: text/plain").expect("text part");
    let calendar = message
        .find("Content-Type: text/calendar")
        .expect("calendar part");
    assert!(
        text < calendar,
        "the calendar part must come last — `multipart/alternative` orders least to most \
         faithful (RFC 2046 §5.1.4), so a client that understands both picks the iTIP object"
    );
}

#[test]
fn the_content_type_carries_the_method_parameter_and_utf8() {
    // The parameter is the whole difference between a scheduling message and a file: RFC
    // 6047 §2.4 requires it, and receiving clients dispatch on it. A `charset` is required
    // too, because `text/*` defaults to US-ASCII while iCalendar defaults to UTF-8.
    let message = assemble(&reply_draft());
    assert!(
        message.contains("Content-Type: text/calendar; charset=utf-8; method=REPLY\r\n"),
        "{message}"
    );
}

#[test]
fn the_method_parameter_is_the_uppercase_itip_spelling() {
    // The engine's canonical `ScheduleMethod` spelling is lowercase (JSCalendar); the
    // iCalendar `METHOD` property inside the object is uppercase (RFC 5545 §3.7.2). RFC
    // 6047 §2.4 compares the two ignoring case, but emitting the object's own spelling
    // means a human reading the raw message sees one value, not two.
    for (method, expected) in [
        (ScheduleMethod::Reply, "method=REPLY"),
        (ScheduleMethod::Request, "method=REQUEST"),
        (ScheduleMethod::Cancel, "method=CANCEL"),
        (ScheduleMethod::Counter, "method=COUNTER"),
    ] {
        let draft = reply_draft().with_calendar(DraftCalendar::new(method, REPLY));
        assert!(assemble(&draft).contains(expected), "{expected}");
    }
}

#[test]
fn the_itip_object_survives_the_transfer_encoding_intact() {
    // §2.5: an iCalendar object must not go out as `7bit`. Its content lines run long and
    // its `charset` is UTF-8, and a transport that folds or re-wraps them corrupts the
    // object — a `UID` broken across lines is a different `UID`. So it is base64-encoded,
    // and this asserts the decoded bytes are byte-identical to what the caller supplied.
    let message = assemble(&reply_draft());
    assert!(
        message.contains("Content-Type: text/calendar; charset=utf-8; method=REPLY\r\nContent-Transfer-Encoding: base64\r\n"),
        "{message}"
    );
    assert_eq!(decoded_part(&message, "Content-Type: text/calendar"), REPLY);
}

#[test]
fn an_ical_with_bare_lf_line_endings_is_normalized_to_crlf() {
    // RFC 5545 §3.1 requires CRLF. A caller assembling the object by hand can easily emit
    // bare LF, and base64 would carry it to the wire verbatim — where a strict parser
    // rejects the whole object. Normalizing is the assembler's job, exactly as it is for
    // the text body.
    let draft = reply_draft().with_calendar(DraftCalendar::new(
        ScheduleMethod::Reply,
        "BEGIN:VCALENDAR\nMETHOD:REPLY\nEND:VCALENDAR\n",
    ));
    assert_eq!(
        decoded_part(&assemble(&draft), "Content-Type: text/calendar"),
        "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n"
    );
}

#[test]
fn an_html_alternative_and_the_itip_object_are_all_siblings() {
    // Three representations of one message, most-faithful last: plain, HTML, iTIP. The
    // calendar part must not displace the HTML one — a recipient whose client cannot read
    // `text/calendar` should still get the formatted answer.
    let draft = reply_draft().with_html_body("<p>Accepted</p>");
    let message = assemble(&draft);
    let plain = message.find("Content-Type: text/plain").expect("plain");
    let html = message.find("Content-Type: text/html").expect("html");
    let calendar = message
        .find("Content-Type: text/calendar")
        .expect("calendar");
    assert!(plain < html && html < calendar, "{message}");
    assert_eq!(
        message.matches("multipart/alternative").count(),
        1,
        "one alternative container, not one per representation: {message}"
    );
}

#[test]
fn an_itip_object_alongside_a_file_nests_the_alternative_inside_mixed() {
    // RFC 6047 §2.4's own note: an enclosed document is a `multipart/mixed` sibling of the
    // *whole* alternative group, never a sibling of the iTIP object — putting a PDF inside
    // `multipart/alternative` would claim it is another rendering of the same message, and
    // a client picking one representation could show the attachment instead of the answer.
    let draft = reply_draft().with_attachment(DraftAttachment::attachment(
        "agenda.pdf",
        "application/pdf",
        b"PDF bytes".to_vec(),
    ));
    let message = assemble(&draft);

    let mixed = message.find("multipart/mixed").expect("mixed");
    let alternative = message.find("multipart/alternative").expect("alternative");
    let calendar = message.find("text/calendar").expect("calendar");
    let pdf = message.find("application/pdf").expect("pdf");
    assert!(mixed < alternative, "{message}");
    assert!(alternative < calendar && calendar < pdf, "{message}");
    assert!(
        message.contains("Content-Disposition: attachment; filename=\"agenda.pdf\"\r\n"),
        "{message}"
    );
    // …and the iTIP part still carries no disposition. It is not a file.
    let part = &message[calendar..pdf];
    assert!(
        !part.contains("Content-Disposition"),
        "the iTIP part must not be dispositioned: {part}"
    );
}

#[test]
fn a_draft_without_a_calendar_is_assembled_exactly_as_before() {
    // Additive: the ordinary send path must not grow a calendar part, an alternative
    // wrapper, or anything else.
    let plain = Draft::new(
        MessageIdHeader::new("plain@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    );
    let message = assemble(&plain);
    assert!(message.contains("Content-Type: text/plain; charset=utf-8\r\n\r\nbody\r\n"));
    assert!(!message.contains("multipart"), "{message}");
    assert!(!message.contains("text/calendar"), "{message}");
}
