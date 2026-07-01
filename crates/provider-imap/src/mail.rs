//! Normalizing parsed IMAP rows into the engine's mail domain model.
//!
//! Pure [`FetchRow`]/[`ListRow`] → [`Message`]/[`Mailbox`] conversion. It maps the
//! three independent axes faithfully (`modeling.md`): the synthesized **object id**
//! `(mailbox, UIDVALIDITY, UID)` is identity (so an IMAP copy in another folder is a
//! *distinct* object with a single membership — the contrast to JMAP's one
//! multi-membership object); the single bound mailbox is the membership; IMAP flags
//! are the keyword state axis. The `Message-ID` header is preserved in the envelope
//! as a hint, never used as identity.
//!
//! Tier-1 metadata only: the raw RFC 5322 source is not materialized here (durable
//! blob storage is a later store sub-step), matching `provider-jmap`.

use std::collections::BTreeSet;

use engine_core::ids::{MailboxId, MessageId, MessageIdHeader, ProviderKey};
use engine_core::mail::{
    EmailAddress, Envelope, Keyword, Mailbox, MailboxRole, Message, SystemKeyword,
};
use engine_core::membership::Memberships;
use engine_core::time::UtcDateTime;
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

use crate::parse::{Address, FetchRow, ListRow};

/// Synthesizes the stable [`MessageId`] key for an IMAP message from its identity
/// triple `(mailbox, UIDVALIDITY, UID)` (RFC 9051 §2.3.1.1).
///
/// The encoding is injective: the numeric `v`/`u` components are decimal and
/// delimited by `:`/`@`, so distinct triples never collide even though the mailbox
/// suffix is free-form. A copy of one message in another folder has a different
/// mailbox component, hence a **different** key — distinct objects, as IMAP
/// requires.
pub(crate) fn message_key(mailbox: &str, uid_validity: u32, uid: u32) -> ProviderKey {
    ProviderKey::new(format!("imap:v{uid_validity}:u{uid}@{mailbox}"))
        .expect("a synthesized IMAP key is never empty")
}

/// The inverse of [`message_key`]: parses `imap:v{validity}:u{uid}@{mailbox}` back
/// into its `(mailbox, UIDVALIDITY, UID)` triple, or `None` on any malformed key.
///
/// A foreign or garbage key (a JMAP key, a truncated value, non-decimal components)
/// yields `None` rather than panicking, so a stale outbox payload is rejected as an
/// invalid edit instead of crashing the mutation path.
pub(crate) fn parse_message_key(key: &str) -> Option<(&str, u32, u32)> {
    let rest = key.strip_prefix("imap:v")?;
    let (validity, rest) = rest.split_once(":u")?;
    let (uid, mailbox) = rest.split_once('@')?;
    if mailbox.is_empty() {
        return None;
    }
    Some((mailbox, validity.parse().ok()?, uid.parse().ok()?))
}

/// Normalizes one `UID FETCH` row into a [`Message`] in the bound mailbox.
///
/// Infallible: malformed sub-fields (an unparseable date, an invalid keyword, a
/// bad `Message-ID`) are dropped rather than failing the whole sync — mail is
/// hostile input.
pub(crate) fn message_from_fetch(
    row: &FetchRow,
    mailbox: &MailboxId,
    uid_validity: u32,
) -> Message {
    let id = MessageId::new(message_key(mailbox.as_str(), uid_validity, row.uid));
    let mut message = Message::new(id, Memberships::of_one(mailbox.clone()));
    message.keywords = flags_to_keywords(&row.flags);
    message.size = row.size;
    message.received_at = row.internal_date.as_deref().and_then(parse_internaldate);
    if let Some(env) = &row.envelope {
        message.envelope = to_envelope(env);
        // The `Date` header instant is not in ENVELOPE as a parsed value; the
        // string form is, but normalizing it is a later refinement. INTERNALDATE
        // (delivery time) is the reliable instant here.
    }
    message.has_attachment = row.has_attachment;
    // `References` is not an ENVELOPE field; it rides a separate
    // `BODY[HEADER.FIELDS (REFERENCES)]` fetch item (the threading chain). The
    // value is the raw header line (`References: <a@x> <b@y>\r\n\r\n`); strip the
    // field name so `extract_message_ids`' bare-value fallback can never mistake
    // `References:` for an id when the header is empty.
    if let Some(raw) = &row.references {
        message.envelope.references = extract_message_ids(strip_header_name(raw));
    }
    message
}

/// Normalizes one `LIST` row into a [`Mailbox`]; `None` for an unusable
/// (`\NonExistent` or empty-named) entry.
pub(crate) fn mailbox_from_list(row: &ListRow) -> Option<Mailbox> {
    if has_attribute(&row.attributes, "NonExistent") {
        return None;
    }
    let id = MailboxId::try_from(row.name.as_str()).ok()?;
    let mut mailbox = Mailbox::new(id, row.name.clone());
    mailbox.role = role_for(&row.name, &row.attributes);
    mailbox.parent = parent_of(&row.name, row.delimiter.as_deref());
    Some(mailbox)
}

/// Maps IMAP flags to engine [`Keyword`]s. The four standard system flags map to
/// their `$`-keywords; `\Deleted` and `\Recent` are deliberately **not** keywords
/// (they belong to IMAP's expunge/session model, per `engine_core::mail::keyword`);
/// other backslash flags are unmapped; a custom keyword passes through if valid.
fn flags_to_keywords(flags: &[String]) -> BTreeSet<Keyword> {
    let mut set = BTreeSet::new();
    for flag in flags {
        if let Some(name) = flag.strip_prefix('\\') {
            let system = match name.to_ascii_lowercase().as_str() {
                "seen" => Some(SystemKeyword::Seen),
                "flagged" => Some(SystemKeyword::Flagged),
                "answered" => Some(SystemKeyword::Answered),
                "draft" => Some(SystemKeyword::Draft),
                _ => None,
            };
            if let Some(system) = system {
                set.insert(Keyword::system(system));
            }
        } else if let Ok(keyword) = Keyword::new(flag.as_str()) {
            set.insert(keyword);
        }
    }
    set
}

/// The inverse of [`flags_to_keywords`]: maps one engine [`Keyword`] to the IMAP
/// flag a `UID STORE` sets. The four standard system keywords (`$seen`/`$flagged`/
/// `$answered`/`$draft`) map back to their backslash system flags; every other
/// keyword — another system keyword (`$forwarded`, …) or a custom one — is emitted
/// as a bare IMAP keyword atom (`Keyword::as_str`, already a valid IMAP atom since
/// the keyword grammar forbids the flag-illegal characters).
pub(crate) fn keyword_to_flag(kw: &Keyword) -> String {
    match kw.as_system() {
        Some(SystemKeyword::Seen) => "\\Seen".to_owned(),
        Some(SystemKeyword::Flagged) => "\\Flagged".to_owned(),
        Some(SystemKeyword::Answered) => "\\Answered".to_owned(),
        Some(SystemKeyword::Draft) => "\\Draft".to_owned(),
        _ => kw.as_str().to_owned(),
    }
}

/// Parses an IMAP `INTERNALDATE` (`"dd-Mon-yyyy hh:mm:ss +zzzz"`, RFC 9051
/// §7.5.2) into a UTC instant, applying the zone offset. `None` on any malformed
/// component.
fn parse_internaldate(raw: &str) -> Option<UtcDateTime> {
    let mut parts = raw.split_whitespace();
    let (date, clock, zone) = (parts.next()?, parts.next()?, parts.next()?);

    let mut date_parts = date.split('-');
    let day: u8 = date_parts.next()?.parse().ok()?;
    let month = month_from_abbreviation(date_parts.next()?)?;
    let year: i32 = date_parts.next()?.parse().ok()?;

    let mut clock_parts = clock.split(':');
    let hour: u8 = clock_parts.next()?.parse().ok()?;
    let minute: u8 = clock_parts.next()?.parse().ok()?;
    let second: u8 = clock_parts.next()?.parse().ok()?;

    let offset = parse_zone(zone)?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;
    let utc = PrimitiveDateTime::new(date, time)
        .assume_offset(offset)
        .to_offset(UtcOffset::UTC);
    UtcDateTime::new(
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
    .ok()
}

/// Parses a `+HHMM` / `-HHMM` zone suffix into a [`UtcOffset`].
fn parse_zone(zone: &str) -> Option<UtcOffset> {
    if zone.len() != 5 {
        return None;
    }
    let sign: i8 = match zone.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours: i8 = zone.get(1..3)?.parse().ok()?;
    let minutes: i8 = zone.get(3..5)?.parse().ok()?;
    UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()
}

/// Maps a three-letter English month abbreviation to a [`Month`].
fn month_from_abbreviation(abbr: &str) -> Option<Month> {
    Some(match abbr.to_ascii_lowercase().as_str() {
        "jan" => Month::January,
        "feb" => Month::February,
        "mar" => Month::March,
        "apr" => Month::April,
        "may" => Month::May,
        "jun" => Month::June,
        "jul" => Month::July,
        "aug" => Month::August,
        "sep" => Month::September,
        "oct" => Month::October,
        "nov" => Month::November,
        "dec" => Month::December,
        _ => return None,
    })
}

/// Maps a parsed IMAP [`Envelope`](crate::parse::Envelope) into the domain
/// [`Envelope`].
fn to_envelope(env: &crate::parse::Envelope) -> Envelope {
    Envelope {
        // Header text may be RFC 2047 encoded (a non-ASCII subject); decode it.
        subject: env.subject.as_deref().map(crate::encoded_word::decode),
        from: to_addresses(&env.from),
        sender: to_addresses(&env.sender),
        reply_to: to_addresses(&env.reply_to),
        to: to_addresses(&env.to),
        cc: to_addresses(&env.cc),
        bcc: to_addresses(&env.bcc),
        in_reply_to: env
            .in_reply_to
            .as_deref()
            .map(extract_message_ids)
            .unwrap_or_default(),
        references: Vec::new(),
        message_id: env
            .message_id
            .as_deref()
            .map(extract_message_ids)
            .unwrap_or_default(),
    }
}

/// Maps IMAP envelope addresses to [`EmailAddress`]es, dropping group markers
/// (entries with no `mailbox`/`host`).
fn to_addresses(addrs: &[Address]) -> Vec<EmailAddress> {
    addrs
        .iter()
        .filter_map(|addr| {
            let mailbox = addr.mailbox.as_deref()?;
            let host = addr.host.as_deref()?;
            let email = format!("{mailbox}@{host}");
            Some(match &addr.name {
                Some(name) => EmailAddress::named(crate::encoded_word::decode(name), email),
                None => EmailAddress::new(email),
            })
        })
        .collect()
}

/// Extracts bracket-less `Message-ID` values from an envelope string, which may
/// hold several `<id@host>` tokens (the `In-Reply-To` case) or one bare value.
/// Invalid or over-long ids are skipped.
fn extract_message_ids(raw: &str) -> Vec<MessageIdHeader> {
    let mut ids = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find('<') {
        let Some(close_rel) = rest[open + 1..].find('>') else {
            break;
        };
        let inner = rest[open + 1..open + 1 + close_rel].trim();
        if let Ok(id) = MessageIdHeader::new(inner) {
            ids.push(id);
        }
        rest = &rest[open + 1 + close_rel + 1..];
    }
    // Fall back to a bare (bracket-less) value only when there were no brackets at
    // all — an empty `<>` must yield nothing, not the literal "<>".
    if ids.is_empty() && !raw.contains('<') {
        ids.extend(MessageIdHeader::new(raw.trim()).ok());
    }
    ids
}

/// Strips a leading `Header-Name:` field-name prefix from a raw header line, so a
/// fetched `BODY[HEADER.FIELDS (...)]` value yields only the field body. Returns the
/// input unchanged when there is no `name:` prefix before the first `<`.
fn strip_header_name(raw: &str) -> &str {
    match (raw.find(':'), raw.find('<')) {
        // A colon that precedes any angle bracket is the field-name separator.
        (Some(colon), open) if open.is_none_or(|o| colon < o) => raw[colon + 1..].trim(),
        _ => raw.trim(),
    }
}

/// Whether the attribute list carries `\<attr>` (case-insensitively).
fn has_attribute(attributes: &[String], attr: &str) -> bool {
    attributes
        .iter()
        .any(|a| a.trim_start_matches('\\').eq_ignore_ascii_case(attr))
}

/// The SPECIAL-USE attributes (RFC 6154/8457) that denote a normalized role,
/// distinct from structural attributes like `\HasNoChildren`.
const SPECIAL_USE: &[&str] = &[
    "sent",
    "drafts",
    "trash",
    "junk",
    "archive",
    "all",
    "flagged",
    "important",
];

/// The normalized role for a folder: `INBOX` by its reserved name, otherwise a
/// SPECIAL-USE attribute (RFC 6154). Non-role attributes (`\HasNoChildren`, …) are
/// ignored.
fn role_for(name: &str, attributes: &[String]) -> Option<MailboxRole> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some(MailboxRole::Inbox);
    }
    attributes
        .iter()
        .find(|attr| {
            SPECIAL_USE.contains(&attr.trim_start_matches('\\').to_ascii_lowercase().as_str())
        })
        .map(|attr| MailboxRole::from_imap_special_use(attr))
}

/// Derives a parent mailbox id from a hierarchical name and its delimiter.
fn parent_of(name: &str, delimiter: Option<&str>) -> Option<MailboxId> {
    let delimiter = delimiter.filter(|d| !d.is_empty())?;
    let index = name.rfind(delimiter)?;
    if index == 0 {
        return None;
    }
    MailboxId::try_from(&name[..index]).ok()
}

#[cfg(test)]
#[path = "mail_tests.rs"]
mod tests;
