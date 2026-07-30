//! Pulling the iMIP scheduling payload — and the addresses the message was delivered to
//! — out of a raw RFC 5322 message.
//!
//! This is the mail side of the mail↔calendar bridge. It reads only what the calendar
//! layer needs to decide "is this an invitation, and is it *for me*?", and it reads it from
//! the **same cached raw source** the body and attachment reads already use, so surfacing
//! an invitation card costs no extra provider fetch.
//!
//! Nothing here interprets the calendar payload: parsing `METHOD`/`ATTENDEE`/`DTSTART` is
//! `engine-ical`'s job, and deciding whether an RSVP is owed is the product's. This module
//! only locates bytes and decodes them to text.

use engine_core::raw::RawMime;
use mail_parser::{
    Address, ContentType, GetHeader, HeaderName, HeaderValue, MessageParser, MessagePart,
    MimeHeaders,
};

/// The iCalendar media type an iMIP scheduling message carries (RFC 6047 §2.4).
const CALENDAR_MEDIA_TYPE: &str = "text/calendar";

/// The non-standard delivery headers that name the address an alias was delivered to. In
/// order of how much we trust them to be *this* mailbox's address.
///
/// These exist because `To:` is not the answer: mail to a distribution list, a `Bcc:`
/// recipient, or an alias frequently does not name the receiving mailbox in any visible
/// header. The MTA that made the final delivery records it here.
const DELIVERY_HEADERS: &[&str] = &["Delivered-To", "X-Original-To", "Envelope-To"];

/// The iMIP scheduling part found in a message, decoded to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarPart {
    /// The part's decoded iCalendar text, ready for `engine-ical` to parse.
    text: String,
    /// The part's declared media type (`text/calendar`, or `application/ics` for the
    /// belt-and-braces copy some senders attach).
    media_type: String,
    /// Whether this came from an **alternative body part** (no `Content-Disposition`) —
    /// the iMIP shape — rather than from a part the sender explicitly attached as a file.
    ///
    /// Load-bearing for the reading view: the body part is the one hidden from the
    /// attachment list, and a sender's deliberately attached `.ics` is the one kept.
    from_inline_body: bool,
}

impl CalendarPart {
    /// The decoded iCalendar text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The part's declared media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Whether the part was an alternative body part rather than an attached file.
    #[must_use]
    pub fn from_inline_body(&self) -> bool {
        self.from_inline_body
    }
}

/// Extracts the iMIP scheduling part from a raw RFC 5322 message, if it has one.
///
/// Prefers the **body** part (undispositioned `text/calendar`), because that is the iMIP
/// payload proper and the one whose `METHOD` carries scheduling intent. Gmail sends both
/// that and a duplicate `Content-Disposition: attachment` copy named `invite.ics`; either
/// would parse, but preferring the body part keeps the choice deterministic and matches
/// what the attachment list hides.
///
/// Falls back to a dispositioned `text/calendar` file when there is no body part — that is
/// how a published `.ics` arrives (and it parses to `METHOD:PUBLISH`, which the product
/// then declines to offer an RSVP for).
///
/// Like every read in this crate, hostile or truncated input yields `None`, never a panic.
#[must_use]
pub fn extract_calendar_part(raw: &RawMime) -> Option<CalendarPart> {
    let message = MessageParser::default().parse(raw.as_bytes())?;
    let mut fallback = None;
    for part in &message.parts {
        if !is_calendar_media_type(part) {
            continue;
        }
        let Some(text) = decoded_text(part) else {
            continue;
        };
        let found = CalendarPart {
            text,
            media_type: declared_media_type(part),
            from_inline_body: part.content_disposition().is_none(),
        };
        if found.from_inline_body {
            return Some(found);
        }
        fallback = fallback.or(Some(found));
    }
    fallback
}

/// Extracts the addresses this message was **delivered to**, most-trusted first.
///
/// This is what makes an invitation to an alias work with no configuration at all: the
/// message reached this mailbox, so an `ATTENDEE` matching an address it was delivered to is
/// the user — even when the account's primary address is something else entirely. Outlook
/// behaves the same way.
///
/// The MTA delivery headers (`Delivered-To`, `X-Original-To`, `Envelope-To`) come first
/// because they name the actual delivery target, then `To:` and `Cc:`. Addresses are
/// returned verbatim; normalizing and comparing them is
/// `engine_core::scheduling::addresses_match`'s job, so there is exactly one implementation
/// of "is this cal-address me?".
///
/// Duplicates are removed case-insensitively while preserving first-seen order. A message
/// that cannot be parsed yields an empty `Vec`.
#[must_use]
pub fn extract_delivery_recipients(raw: &RawMime) -> Vec<String> {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return Vec::new();
    };

    let mut out: Vec<String> = Vec::new();
    let mut push = |address: &str| {
        let trimmed = address.trim();
        if trimmed.is_empty() {
            return;
        }
        if !out.iter().any(|seen| seen.eq_ignore_ascii_case(trimmed)) {
            out.push(trimmed.to_owned());
        }
    };

    for name in DELIVERY_HEADERS {
        for value in message.header_values(*name) {
            collect_addresses(value, &mut push);
        }
    }
    for header in [HeaderName::To, HeaderName::Cc] {
        if let Some(value) = message.root_part().headers.header_value(&header) {
            collect_addresses(value, &mut push);
        }
    }
    out
}

/// Appends every address in `value` to `push`, handling both address-typed headers (`To:`,
/// `Cc:`) and the plain-text delivery headers, which are not parsed as addresses.
fn collect_addresses(value: &HeaderValue<'_>, push: &mut impl FnMut(&str)) {
    match value {
        HeaderValue::Address(Address::List(list)) => {
            for addr in list {
                if let Some(email) = &addr.address {
                    push(email.as_ref());
                }
            }
        }
        HeaderValue::Address(Address::Group(groups)) => {
            for group in groups {
                for addr in &group.addresses {
                    if let Some(email) = &addr.address {
                        push(email.as_ref());
                    }
                }
            }
        }
        HeaderValue::Text(text) => push(strip_angle_brackets(text.as_ref())),
        HeaderValue::TextList(list) => {
            for text in list {
                push(strip_angle_brackets(text.as_ref()));
            }
        }
        _ => {}
    }
}

/// Strips the `<…>` a delivery header often wraps its address in.
fn strip_angle_brackets(value: &str) -> &str {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
}

/// Whether `part` declares an iCalendar media type — `text/calendar`, or the
/// `application/ics` some senders use for an attached copy.
fn is_calendar_media_type(part: &MessagePart<'_>) -> bool {
    let media_type = declared_media_type(part);
    media_type.eq_ignore_ascii_case(CALENDAR_MEDIA_TYPE)
        || media_type.eq_ignore_ascii_case("application/ics")
}

/// The part's `Content-Type` media type with parameters stripped, lowercased.
fn declared_media_type(part: &MessagePart<'_>) -> String {
    part.content_type()
        .map_or_else(String::new, |ct| media_type_of(ct).to_ascii_lowercase())
}

fn media_type_of(content_type: &ContentType<'_>) -> String {
    match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_owned(),
    }
}

/// The part's content-transfer- and charset-decoded text.
///
/// mail-parser decodes `base64`/`quoted-printable` and the declared charset for text parts,
/// so an `iso-8859-1` Outlook payload and a `quoted-printable` Gmail one both come back as
/// UTF-8. A part typed as binary (some senders label the attached copy
/// `application/ics`, which is not a text type) is decoded lossily from its bytes rather
/// than dropped — iCalendar is ASCII-dominated, so this recovers a payload that would
/// otherwise be invisible.
fn decoded_text(part: &MessagePart<'_>) -> Option<String> {
    use mail_parser::PartType;
    let text = match &part.body {
        PartType::Text(text) | PartType::Html(text) => text.as_ref().to_owned(),
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => {
            String::from_utf8_lossy(bytes.as_ref()).into_owned()
        }
        PartType::Message(_) | PartType::Multipart(_) => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: &[u8]) -> RawMime {
        RawMime::new(bytes)
    }

    /// The Outlook shape: `text/calendar` inside `multipart/alternative`, no disposition,
    /// no filename, `base64`, `iso-8859-1`.
    const OUTLOOK: &[u8] = b"To: dennis@test.local\r\n\
        Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
        --a\r\nContent-Type: text/plain; charset=\"iso-8859-1\"\r\n\r\nWhen: today\r\n\
        --a\r\nContent-Type: text/calendar; charset=\"iso-8859-1\"; method=REQUEST\r\n\
        Content-Transfer-Encoding: base64\r\n\r\n\
        QkVHSU46VkNBTEVOREFSDQpNRVRIT0Q6UkVRVUVTVA0KRU5EOlZDQUxFTkRBUg0K\r\n\
        --a--\r\n";

    /// The Gmail shape: the body part **and** a dispositioned duplicate.
    const GMAIL: &[u8] = b"To: dennis@test.local\r\n\
        Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
        --m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
        --a\r\nContent-Type: text/plain\r\n\r\nnote\r\n\
        --a\r\nContent-Type: text/calendar; charset=UTF-8; method=REQUEST\r\n\
        Content-Transfer-Encoding: quoted-printable\r\n\r\n\
        BEGIN:VCALENDAR=0D=0AMETHOD:REQUEST=0D=0AEND:VCALENDAR=0D=0A\r\n\
        --a--\r\n\
        --m\r\nContent-Type: application/ics; name=\"invite.ics\"\r\n\
        Content-Disposition: attachment; filename=\"invite.ics\"\r\n\r\n\
        BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n\
        --m--\r\n";

    /// A published `.ics`: an attachment only, no body part.
    const PUBLISHED: &[u8] = b"To: residents@test.local\r\n\
        Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
        --m\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
        --m\r\nContent-Type: text/calendar; charset=UTF-8\r\n\
        Content-Disposition: attachment; filename=\"agenda.ics\"\r\n\r\n\
        BEGIN:VCALENDAR\r\nMETHOD:PUBLISH\r\nEND:VCALENDAR\r\n\
        --m--\r\n";

    #[test]
    fn finds_an_undispositioned_body_part_and_decodes_base64() {
        let part = extract_calendar_part(&raw(OUTLOOK)).expect("calendar part");
        assert!(part.from_inline_body(), "the iMIP shape is a body part");
        assert_eq!(part.media_type(), "text/calendar");
        assert!(part.text().contains("METHOD:REQUEST"));
    }

    #[test]
    fn prefers_the_body_part_over_a_duplicate_attachment() {
        // Gmail sends both. Either parses, but the choice must be deterministic — and it
        // must agree with the part the attachment list hides.
        let part = extract_calendar_part(&raw(GMAIL)).expect("calendar part");
        assert!(part.from_inline_body());
        assert_eq!(part.media_type(), "text/calendar");
        assert!(part.text().contains("METHOD:REQUEST"));
    }

    #[test]
    fn falls_back_to_an_attached_ics_when_there_is_no_body_part() {
        let part = extract_calendar_part(&raw(PUBLISHED)).expect("calendar part");
        assert!(
            !part.from_inline_body(),
            "a published .ics is a real attachment"
        );
        assert!(part.text().contains("METHOD:PUBLISH"));
    }

    #[test]
    fn a_message_with_no_calendar_part_yields_none() {
        let plain = b"To: a@test.local\r\nContent-Type: text/plain\r\n\r\nhello\r\n";
        assert!(extract_calendar_part(&raw(plain)).is_none());
    }

    #[test]
    fn hostile_input_never_panics() {
        for bytes in [
            &b""[..],
            &b"Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n--a\r\n"[..],
            &b"Content-Type: text/calendar\r\nContent-Transfer-Encoding: base64\r\n\r\n!!!!\r\n"[..],
            &b"\xff\xfe\x00garbage"[..],
        ] {
            let _ = extract_calendar_part(&raw(bytes));
            let _ = extract_delivery_recipients(&raw(bytes));
        }
    }

    // --- delivery recipients (the zero-configuration alias case) ------------------

    #[test]
    fn delivery_headers_come_before_to_and_cc() {
        // The reported case: an invitation addressed to a list, delivered to an alias. The
        // alias appears *only* in Delivered-To, and it is the address that identifies the
        // user — so it must be offered first.
        let msg = b"Delivered-To: info@test.local\r\n\
            To: everyone@test.local\r\n\
            Cc: someone@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        assert_eq!(
            extract_delivery_recipients(&raw(msg)),
            vec![
                "info@test.local".to_owned(),
                "everyone@test.local".to_owned(),
                "someone@test.local".to_owned(),
            ]
        );
    }

    #[test]
    fn all_three_delivery_headers_are_read_and_bracket_wrapped_values_unwrapped() {
        let msg = b"X-Original-To: <original@test.local>\r\n\
            Envelope-To: envelope@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        let found = extract_delivery_recipients(&raw(msg));
        assert!(
            found.contains(&"original@test.local".to_owned()),
            "{found:?}"
        );
        assert!(
            found.contains(&"envelope@test.local".to_owned()),
            "{found:?}"
        );
    }

    #[test]
    fn duplicates_are_removed_case_insensitively_keeping_first_seen_order() {
        let msg = b"Delivered-To: Info@Test.Local\r\n\
            To: info@test.local, other@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        assert_eq!(
            extract_delivery_recipients(&raw(msg)),
            vec!["Info@Test.Local".to_owned(), "other@test.local".to_owned()],
            "the delivery header's spelling wins, and the To: repeat is dropped"
        );
    }

    #[test]
    fn a_message_with_no_recipients_yields_empty() {
        let msg = b"From: a@test.local\r\nContent-Type: text/plain\r\n\r\nhi\r\n";
        assert!(extract_delivery_recipients(&raw(msg)).is_empty());
    }
}
