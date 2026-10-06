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

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ops::Range,
};

use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Envelope, Keyword, Mailbox, MailboxRole, Message, SystemKeyword},
    membership::Memberships,
    time::UtcDateTime,
};
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
    message.has_attachment = row.has_attachment.unwrap_or(false);
    // `References` is not an ENVELOPE field; it rides the fetched header section (the
    // threading chain), beside other fields whose values may hold `<…>` too.
    if let Some(references) = row
        .header_fields
        .as_deref()
        .and_then(|fields| header_field(fields, "References"))
    {
        message.envelope.references = extract_message_ids(&references);
    }
    message
}

/// Normalizes one `LIST` row into a [`Mailbox`]; `None` for an empty-named entry.
///
/// A `\Noselect` or `\NonExistent` row is a level of the hierarchy rather than a folder, and
/// comes back with [`Mailbox::selectable`] cleared. Whether a `\NonExistent` one belongs in the
/// list at all depends on the rest of it, which is [`mailboxes_from_list`]'s question.
///
/// `modified_utf7` says how this session's wire encodes names — IMAP4rev1 does, IMAP4rev2
/// does not (RFC 9051 §5.1) — and must come from what the session *negotiated*, never from
/// a capability the server merely advertised. Decoding unconditionally is not safe on a
/// rev2 name: `R&AD-` is a mailbox called `R&AD-` there, and a shift sequence in rev1.
///
/// The **decoded full path is the id**, so a mailbox keeps one identity whichever revision a
/// session negotiates, and [`crate::transport`] puts the wire form back on outgoing commands:
/// the id is what `SELECT`/`APPEND` take and what every message key embeds.
///
/// The **display name is the folder's own name**, the last segment of that path, with the
/// nesting carried by [`Mailbox::parent`] instead. IMAP is the only transport that names a
/// folder by where it sits (JMAP, Graph and Gmail all give the segment alone), and a host
/// drawing a tree would otherwise indent `Archive/2024` underneath `Archive`. The path is
/// still there, as the chain of parents.
///
/// Role matching runs on the **full** path: `INBOX` is a reserved name at the top level, and a
/// folder called `Sent` inside another one is not the account's Sent.
pub(crate) fn mailbox_from_list(row: &ListRow, modified_utf7: bool) -> Option<Mailbox> {
    let name = if modified_utf7 {
        crate::utf7::decode(&row.name)
    } else {
        row.name.clone()
    };
    let id = mailbox_id_of(&name)?;
    let delimiter = row.delimiter.as_deref();
    let mut mailbox = Mailbox::new(id, leaf_of(&name, delimiter));
    mailbox.role = role_for(&name, &row.attributes);
    mailbox.parent = parent_of(&name, delimiter);
    // RFC 3501 §7.2.2 `\Noinferiors`; with no delimiter there is no level to make.
    mailbox.accepts_children =
        delimiter.is_some() && !has_attribute(&row.attributes, "Noinferiors");
    mailbox.selectable = !is_hierarchy_level(&row.attributes);
    Some(mailbox)
}

/// Normalizes a whole `LIST` response, keeping each mailbox beside the row it came from.
///
/// A `\NonExistent` row is kept only while a listed folder sits beneath it. Dovecot answers the
/// extended `LIST` this way for the parent of a folder made inside one nobody made (the plain
/// `LIST` says `\Noselect`), and dropping it would leave that folder's parent missing from the
/// tree. Without a folder beneath it, it names nothing a host could show.
pub(crate) fn mailboxes_from_list(
    rows: &[ListRow],
    modified_utf7: bool,
) -> Vec<(&ListRow, Mailbox)> {
    let mapped: Vec<_> = rows
        .iter()
        .filter_map(|row| Some((row, mailbox_from_list(row, modified_utf7)?)))
        .collect();
    let by_id: HashMap<&MailboxId, &Mailbox> = mapped.iter().map(|(_, m)| (&m.id, m)).collect();
    let mut holding: HashSet<&MailboxId> = HashSet::new();
    for (row, mailbox) in &mapped {
        if has_attribute(&row.attributes, "NonExistent") {
            continue;
        }
        let mut above = mailbox.parent.as_ref();
        while let Some(parent) = above
            && holding.insert(parent)
        {
            above = by_id.get(parent).and_then(|m| m.parent.as_ref());
        }
    }
    let holding: HashSet<MailboxId> = holding.into_iter().cloned().collect();
    mapped
        .into_iter()
        .filter(|(row, mailbox)| {
            !has_attribute(&row.attributes, "NonExistent") || holding.contains(&mailbox.id)
        })
        .collect()
}

/// Whether a `LIST` row names a level of the hierarchy that cannot be selected. RFC 9051
/// §7.3.1: `\NonExistent` implies `\Noselect`.
pub(crate) fn is_hierarchy_level(attributes: &[String]) -> bool {
    has_attribute(attributes, "Noselect") || has_attribute(attributes, "NonExistent")
}

/// Maps IMAP flags to engine [`Keyword`]s. The four standard system flags map to
/// their `$`-keywords; `\Deleted` and `\Recent` are deliberately **not** keywords
/// (they belong to IMAP's expunge/session model, per `engine_core::mail::keyword`);
/// other backslash flags are unmapped; a custom keyword passes through if valid.
pub(crate) fn flags_to_keywords(flags: &[String]) -> BTreeSet<Keyword> {
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
        subject: env
            .subject
            .as_deref()
            .map(engine_mime::encoded_word::decode),
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
                Some(name) => EmailAddress::named(engine_mime::encoded_word::decode(name), email),
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

/// The unfolded value of the field `name` in a raw header section, or `None` when the
/// section has no such field. A line that starts with whitespace continues the field
/// before it (RFC 5322 §2.2.3).
fn header_field(section: &str, name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for line in section.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(value) = value.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if value.is_some() {
            break;
        }
        let Some((field, rest)) = line.split_once(':') else {
            continue;
        };
        if field.trim().eq_ignore_ascii_case(name) {
            value = Some(rest.trim().to_owned());
        }
    }
    value
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
    mailbox_id_of(&name[..split_at(name, delimiter)?.start])
}

/// The id of the mailbox a full path names: the path itself, except that the inbox is `INBOX`
/// however the server spells it. RFC 9051 §5.1 makes the reserved name case-insensitive, and a
/// host binds the inbox by it, so an id in the server's spelling (Yahoo lists `Inbox`) would be
/// a second scope for the same mailbox.
fn mailbox_id_of(path: &str) -> Option<MailboxId> {
    let path = if path.eq_ignore_ascii_case("INBOX") {
        "INBOX"
    } else {
        path
    };
    MailboxId::try_from(path).ok()
}

/// The folder's own name: whatever follows the last delimiter in a hierarchical name, and the
/// whole name for a top-level folder or a server that declares no delimiter (a flat namespace).
///
/// A name ending in the delimiter would otherwise leave nothing to show, so it keeps the whole
/// name: no `LIST` row should carry one, and an empty row in a folder pane is worse than an odd
/// one.
fn leaf_of(name: &str, delimiter: Option<&str>) -> String {
    let leaf = split_at(name, delimiter)
        .map(|split| &name[split.end..])
        .filter(|leaf| !leaf.is_empty());
    leaf.unwrap_or(name).to_owned()
}

/// Where a hierarchical name splits into parent and leaf: the byte range of the last delimiter,
/// or `None` when the name has no parent.
///
/// A delimiter at offset `0` is not a split. A rooted namespace lists names beginning with the
/// separator (`/Archive`, `.Archive`), and reading that one as a split would hand the folder an
/// empty-named parent no `LIST` row ever names. A name that merely *contains* the separator is
/// an ordinary child: `INBOX.Archive` on Courier really does sit inside the inbox.
///
/// A delimiter the server **escaped** is split on all the same, deliberately. IMAP defines no
/// way to put the delimiter inside a name, so a server holding a folder called `29/01/2021` has
/// to invent one, and Proton Mail Bridge's is a preceding backslash. Reading that escape back
/// would recover the name the person typed, and it is still the wrong answer: Bridge also lists
/// a row per fragment, so honouring the escape turns one folder into three siblings. Thunderbird
/// and Apple Mail both split, showing the chain with its intermediates greyed out, and a folder
/// pane that agrees with every other client beats one that is privately more correct.
fn split_at(name: &str, delimiter: Option<&str>) -> Option<Range<usize>> {
    let delimiter = delimiter.filter(|d| !d.is_empty())?;
    let index = name.rfind(delimiter).filter(|index| *index > 0)?;
    Some(index..index + delimiter.len())
}

#[cfg(test)]
#[path = "mail_tests.rs"]
mod tests;
