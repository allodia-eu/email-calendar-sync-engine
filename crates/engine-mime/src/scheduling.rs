//! Pulling the iMIP scheduling payload, and who the message was delivered and addressed to,
//! out of a raw RFC 5322 message.
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
use mail_parser::{ContentType, MessageParser, MessagePart, PartKind};

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
/// Only the message's own parts are read: an invitation inside a message it forwards
/// (`message/rfc822`) was addressed to whoever received *that* message.
///
/// Like every read in this crate, hostile or truncated input yields `None`, never a panic.
#[must_use]
pub fn extract_calendar_part(raw: &RawMime) -> Option<CalendarPart> {
    let message = MessageParser::default().parse(raw.as_bytes())?;
    let mut fallback = None;
    for part in message.root().parts() {
        if !is_calendar_media_type(&part) {
            continue;
        }
        let Some(text) = decoded_text(&part) else {
            continue;
        };
        let found = CalendarPart {
            text,
            media_type: declared_media_type(&part),
            from_inline_body: part.content_disposition().is_none(),
        };
        if found.from_inline_body {
            return Some(found);
        }
        fallback = fallback.or(Some(found));
    }
    fallback
}

/// The addresses a message names as its recipients, in the two kinds a host must trust
/// differently.
///
/// `delivered_to` is what the final MTA recorded delivering this copy to, so an address there
/// is **this mailbox**: an invitation to an alias is recognised through it with no
/// configuration at all. `addressed_to` is what the sender wrote, so an address there is
/// **someone** the message went to, not necessarily this mailbox: an organiser addresses an
/// invitation to every attendee, so a host that took each `To:` address as the user's own would
/// answer as whichever colleague's `ATTENDEE` it met first.
///
/// Addresses are returned verbatim; normalising and comparing them is
/// `engine_core::scheduling::addresses_match`'s job, so there is exactly one implementation of
/// "is this cal-address me?". Each list drops duplicates case-insensitively and keeps first-seen
/// order; an address may appear in both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageRecipients {
    /// `Delivered-To`, then `X-Original-To`, then `Envelope-To`. Empty on mail that made no
    /// MTA hop, such as an invitation sent inside one Exchange tenant.
    pub delivered_to: Vec<String>,
    /// `To:`, then `Cc:`, with any group flattened into its mailboxes.
    pub addressed_to: Vec<String>,
}

/// Extracts who `raw` was delivered to and who it was addressed to.
///
/// A message that cannot be parsed yields two empty lists.
#[must_use]
pub fn extract_recipients(raw: &RawMime) -> MessageRecipients {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return MessageRecipients::default();
    };

    let mut found = MessageRecipients::default();
    // Not address fields in RFC 5322, so mail-parser keeps them as text.
    for name in DELIVERY_HEADERS {
        for header in message.headers().all(*name) {
            if let Some(text) = header.value().as_text() {
                push_unique(&mut found.delivered_to, strip_angle_brackets(text));
            }
        }
    }
    for list in [message.to(), message.cc()].into_iter().flatten() {
        for email in list.mailboxes().filter_map(|mailbox| mailbox.address()) {
            push_unique(&mut found.addressed_to, email);
        }
    }
    found
}

/// Appends `address`, trimmed, unless it is blank or already present in any case.
fn push_unique(out: &mut Vec<String>, address: &str) {
    let trimmed = address.trim();
    if trimmed.is_empty() || out.iter().any(|seen| seen.eq_ignore_ascii_case(trimmed)) {
        return;
    }
    out.push(trimmed.to_owned());
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
        .map_or_default(|ct| media_type_of(ct).to_ascii_lowercase())
}

fn media_type_of(content_type: ContentType<'_>) -> String {
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
    let text = match part.kind() {
        PartKind::Text | PartKind::Html => part.text()?.into_owned(),
        PartKind::Binary | PartKind::InlineBinary => {
            String::from_utf8_lossy(&part.decoded()).into_owned()
        }
        PartKind::Message(_) | PartKind::Multipart => return None,
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
    fn an_invitation_inside_a_forwarded_message_is_not_this_messages() {
        // mail-parser 1.0 numbers the parts of nested messages with the root's; only the
        // root message's own parts may yield the card.
        let forwarded = b"To: a@test.local\r\n\
            Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
            --m\r\nContent-Type: text/plain\r\n\r\nfyi\r\n\
            --m\r\nContent-Type: message/rfc822\r\n\r\n\
            Content-Type: text/calendar; method=REQUEST\r\n\r\n\
            BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n\
            --m--\r\n";
        assert!(extract_calendar_part(&raw(forwarded)).is_none());
    }

    #[test]
    fn a_group_in_to_is_flattened_into_its_mailboxes() {
        let msg = b"To: Team: one@test.local, two@test.local;, three@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        assert_eq!(
            extract_recipients(&raw(msg)).addressed_to,
            ["one@test.local", "two@test.local", "three@test.local"]
        );
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
            let _ = extract_recipients(&raw(bytes));
        }
    }

    // --- delivery recipients (the zero-configuration alias case) ------------------

    #[test]
    fn delivery_headers_and_to_cc_are_reported_apart() {
        // The reported case: an invitation addressed to a list, delivered to an alias. The
        // alias appears *only* in Delivered-To, and it is the address that identifies the
        // user; `To:` and `Cc:` name who the sender wrote to, which is not the same claim.
        let msg = b"Delivered-To: info@test.local\r\n\
            To: everyone@test.local\r\n\
            Cc: someone@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        let found = extract_recipients(&raw(msg));
        assert_eq!(found.delivered_to, ["info@test.local"]);
        assert_eq!(
            found.addressed_to,
            ["everyone@test.local", "someone@test.local"]
        );
    }

    #[test]
    fn an_invitation_to_every_attendee_delivers_to_none_of_them_in_particular() {
        // How Exchange addresses a meeting: every attendee in `To:`, nothing else naming the
        // recipient inside the tenant. None of these addresses is evidence of which attendee
        // this mailbox is, so none may come back as a delivery.
        let msg = b"To: board@test.local, carol@test.local, me@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        let found = extract_recipients(&raw(msg));
        assert!(found.delivered_to.is_empty(), "{found:?}");
        assert_eq!(
            found.addressed_to,
            ["board@test.local", "carol@test.local", "me@test.local"]
        );
    }

    #[test]
    fn all_three_delivery_headers_are_read_and_bracket_wrapped_values_unwrapped() {
        let msg = b"X-Original-To: <original@test.local>\r\n\
            Envelope-To: envelope@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        let found = extract_recipients(&raw(msg)).delivered_to;
        assert_eq!(found, ["original@test.local", "envelope@test.local"]);
    }

    #[test]
    fn duplicates_are_removed_case_insensitively_within_each_list() {
        let msg = b"Delivered-To: Info@Test.Local\r\n\
            Delivered-To: info@test.local\r\n\
            To: info@test.local, other@test.local, OTHER@test.local\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n";
        let found = extract_recipients(&raw(msg));
        assert_eq!(
            found.delivered_to,
            ["Info@Test.Local"],
            "first spelling wins"
        );
        assert_eq!(
            found.addressed_to,
            ["info@test.local", "other@test.local"],
            "an address delivered to is still one the sender wrote to"
        );
    }

    #[test]
    fn an_attached_copy_typed_as_binary_is_read_as_text() {
        // `application/ics` is not a text type, so the part is binary; iCalendar is ASCII.
        let attached_only = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
            --m\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
            --m\r\nContent-Type: application/ics; name=\"invite.ics\"\r\n\
            Content-Disposition: attachment; filename=\"invite.ics\"\r\n\
            Content-Transfer-Encoding: base64\r\n\r\n\
            QkVHSU46VkNBTEVOREFSDQpNRVRIT0Q6UkVRVUVTVA0KRU5EOlZDQUxFTkRBUg0K\r\n\
            --m--\r\n";
        let part = extract_calendar_part(&raw(attached_only)).expect("calendar part");
        assert_eq!(part.media_type(), "application/ics");
        assert!(!part.from_inline_body());
        assert!(part.text().contains("METHOD:REQUEST"));
    }

    #[test]
    fn a_blank_delivery_header_names_no_one() {
        // mail-parser 1.0 gives a present but blank field the value "" rather than none.
        let msg = b"Delivered-To: \r\nTo: a@test.local\r\nContent-Type: text/plain\r\n\r\nhi\r\n";
        let found = extract_recipients(&raw(msg));
        assert!(found.delivered_to.is_empty(), "{found:?}");
        assert_eq!(found.addressed_to, ["a@test.local"]);
    }

    #[test]
    fn a_message_with_no_recipients_yields_empty() {
        let msg = b"From: a@test.local\r\nContent-Type: text/plain\r\n\r\nhi\r\n";
        assert_eq!(extract_recipients(&raw(msg)), MessageRecipients::default());
    }
}
