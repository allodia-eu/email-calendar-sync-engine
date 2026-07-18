//! Normalizing Gmail `label` and `message` JSON into the engine's domain model.
//!
//! Pure `serde_json::Value` → [`Mailbox`]/[`Message`] conversion, unit-tested offline
//! against captured fixtures. Gmail's shape is the multi-membership one `engine-core`
//! was built for (`modeling.md`):
//!
//! - A message's `labelIds` is its **membership** across labels (a message is in `INBOX` *and*
//!   `SENT` *and* a custom label at once — not one folder like Graph/IMAP).
//! - The **keyword-only** system labels `UNREAD`/`STARRED` are state, not a place, so they map to
//!   the keyword axis (`$seen` is the *absence* of `UNREAD`; `STARRED` → `$flagged`) and are
//!   **excluded** from membership and from the label list. `DRAFT` is both a place (the Drafts
//!   label) *and* the `$draft` state.
//! - `threadId` is the provider-assigned thread ([`ThreadProvenance::ProviderAssigned`]), never
//!   re-grouped by local derivation.
//! - The `Message-Id` header is preserved (bracket-stripped) as a threading hint, never identity —
//!   the Gmail message `id` is identity.
//!
//! Gmail returns header values as raw RFC 5322 strings (`From: Name <addr>`), unlike
//! Graph's structured `{name,address}`, so [`parse_addresses`] does a pragmatic
//! address-list parse. `internalDate` is epoch-millis (→ `received_at`); the `Date`
//! header is RFC 2822 (→ `sent_at`).
//!
//! [`ThreadProvenance::ProviderAssigned`]: engine_core::mail::ThreadProvenance::ProviderAssigned

use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ThreadId},
    mail::{EmailAddress, Keyword, Mailbox, MailboxRole, Message, SystemKeyword, ThreadRef},
    membership::Memberships,
    time::UtcDateTime,
    version::RevisionTokens,
};
use serde_json::Value;

use crate::{
    error::GoogleError,
    json::{opt_str, req_str, wrap_id},
};

/// The Gmail system labels that are **keyword state**, not a membership place: they map
/// onto the message's keyword axis and are never emitted as mailboxes.
pub(crate) const KEYWORD_LABELS: &[&str] = &["UNREAD", "STARRED"];

/// The synthetic mailbox id every message with no other label falls back into — Gmail's
/// implicit "All Mail" (an archived, uncategorized message carries no folder-like
/// label, but the engine's [`Memberships`] must be non-empty). Gmail exposes no label id
/// for All Mail, so the adapter reserves this one and emits the matching mailbox in the
/// label list ([`crate::fetch::labels`]).
pub(crate) const ALL_MAIL_ID: &str = "ALL_MAIL";

/// The header set the metadata fetch requests (`metadataHeaders`) — exactly the fields
/// [`message_from_json`] reads. Keeps the captured payload minimal and deterministic.
pub(crate) const METADATA_HEADERS: &[&str] =
    &["From", "To", "Cc", "Bcc", "Subject", "Date", "Message-Id"];

/// The normalized [`MailboxRole`] for a Gmail system label id, or `None` for a
/// category/chat/custom label (which is a roleless mailbox).
fn label_role(id: &str) -> Option<MailboxRole> {
    match id {
        "INBOX" => Some(MailboxRole::Inbox),
        "SENT" => Some(MailboxRole::Sent),
        "DRAFT" => Some(MailboxRole::Drafts),
        "TRASH" => Some(MailboxRole::Trash),
        "SPAM" => Some(MailboxRole::Junk),
        "IMPORTANT" => Some(MailboxRole::Important),
        _ => None,
    }
}

/// Normalizes one Gmail `label` into a [`Mailbox`], or `None` for a keyword-only label
/// (`UNREAD`/`STARRED`), which is state rather than a place.
///
/// # Errors
///
/// Returns [`GoogleError::Protocol`] if the label lacks a usable `id`.
pub(crate) fn label_from_json(value: &Value) -> Result<Option<Mailbox>, GoogleError> {
    let id = req_str(value, "id")?;
    if KEYWORD_LABELS.contains(&id) {
        return Ok(None);
    }
    // System labels have an uppercase id as their name; custom labels carry the user's.
    let name = opt_str(value, "name").unwrap_or(id).to_owned();
    let mut mailbox = Mailbox::new(wrap_id(MailboxId::try_from(id), "label id")?, name);
    mailbox.role = label_role(id);
    Ok(Some(mailbox))
}

/// The synthetic "All Mail" mailbox the label list appends (see [`ALL_MAIL_ID`]).
pub(crate) fn all_mail_mailbox() -> Mailbox {
    let mut mailbox = Mailbox::new(
        MailboxId::try_from(ALL_MAIL_ID).expect("reserved id is valid"),
        "All Mail",
    );
    mailbox.role = Some(MailboxRole::All);
    mailbox
}

/// Normalizes one **full** Gmail `message` (a `metadata`/`full` `get`, or a re-fetched
/// changed message) into a [`Message`]. Never fed a history *partial* (id + labelIds
/// only); those are re-fetched first.
///
/// # Errors
///
/// Returns [`GoogleError::Protocol`] if `id`/`threadId` is missing or a value is
/// malformed.
pub(crate) fn message_from_json(value: &Value) -> Result<Message, GoogleError> {
    let id = wrap_id(MessageId::try_from(req_str(value, "id")?), "message id")?;
    let labels = label_ids(value);

    let memberships = memberships_of(&labels)?;
    let mut message = Message::new(id, memberships);

    message.thread = Some(ThreadRef::provider_assigned(wrap_id(
        ThreadId::try_from(req_str(value, "threadId")?),
        "thread id",
    )?));

    // Gmail models read/star/draft as labels; the engine models them as keywords.
    if !labels.iter().any(|l| l == "UNREAD") {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Seen));
    }
    if labels.iter().any(|l| l == "STARRED") {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Flagged));
    }
    if labels.iter().any(|l| l == "DRAFT") {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Draft));
    }

    message.received_at = internal_date(value)?;
    message.preview = opt_str(value, "snippet").map(snippet);
    message.has_attachment = has_attachment(value);
    // Gmail has no per-message revision token: the message id is immutable and the
    // account-global historyId is the sync cursor, not a per-object version.
    message.revisions = RevisionTokens::none();

    let headers = value.get("payload").and_then(|p| p.get("headers"));
    let envelope = &mut message.envelope;
    envelope.subject = header(headers, "Subject").map(str::to_owned);
    envelope.from = parse_addresses(header(headers, "From"));
    envelope.sender = parse_addresses(header(headers, "Sender"));
    envelope.to = parse_addresses(header(headers, "To"));
    envelope.cc = parse_addresses(header(headers, "Cc"));
    envelope.bcc = parse_addresses(header(headers, "Bcc"));
    envelope.message_id = message_id_header(header(headers, "Message-Id"))?
        .into_iter()
        .collect();
    message.sent_at = header(headers, "Date").and_then(parse_rfc2822);

    Ok(message)
}

/// The `labelIds` array as owned strings (empty when absent).
fn label_ids(value: &Value) -> Vec<String> {
    value
        .get("labelIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The membership mailboxes for a label set: every label that is not keyword-only,
/// falling back to the synthetic All Mail when none remain (an archived, uncategorized
/// message) so the non-empty [`Memberships`] invariant holds.
fn memberships_of(labels: &[String]) -> Result<Memberships<MailboxId>, GoogleError> {
    let mut ids = Vec::new();
    for label in labels {
        if KEYWORD_LABELS.contains(&label.as_str()) {
            continue;
        }
        ids.push(wrap_id(MailboxId::try_from(label.as_str()), "label id")?);
    }
    if ids.is_empty() {
        return Ok(Memberships::of_one(
            MailboxId::try_from(ALL_MAIL_ID).expect("reserved id is valid"),
        ));
    }
    Memberships::new(ids).map_err(|e| GoogleError::protocol(format!("membership: {e}")))
}

/// The first header whose name matches `key` case-insensitively (`Message-Id` vs
/// `Message-ID`), or `None`.
fn header<'a>(headers: Option<&'a Value>, key: &str) -> Option<&'a str> {
    headers
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|h| {
            h.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(key))
        })
        .and_then(|h| h.get("value").and_then(Value::as_str))
}

/// Parses the epoch-millis `internalDate` into a UTC instant (Gmail's message-received
/// time), or `None` when absent.
fn internal_date(value: &Value) -> Result<Option<UtcDateTime>, GoogleError> {
    let Some(raw) = opt_str(value, "internalDate") else {
        return Ok(None);
    };
    let millis: i64 = raw
        .parse()
        .map_err(|e| GoogleError::protocol(format!("bad internalDate {raw:?}: {e}")))?;
    let odt = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .map_err(|e| GoogleError::protocol(format!("internalDate out of range: {e}")))?;
    utc_from_offset(odt).map(Some)
}

/// Parses an RFC 2822 `Date` header into a UTC instant, or `None` if unparseable.
fn parse_rfc2822(raw: &str) -> Option<UtcDateTime> {
    let odt =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822).ok()?;
    utc_from_offset(odt).ok()
}

/// Converts a `time::OffsetDateTime` to the engine's [`UtcDateTime`] (normalizing to
/// UTC).
fn utc_from_offset(odt: time::OffsetDateTime) -> Result<UtcDateTime, GoogleError> {
    let utc = odt.to_offset(time::UtcOffset::UTC);
    UtcDateTime::new(
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
    .map_err(|e| GoogleError::protocol(format!("bad instant: {e}")))
}

/// Truncates a snippet to the model's 256-character bound.
fn snippet(text: &str) -> String {
    text.chars().take(256).collect()
}

/// Whether any `payload` part carries a non-empty `filename` (an attachment). The
/// `metadata` format omits the parts tree, so this is `false` there; the `full` format
/// exposes it.
fn has_attachment(value: &Value) -> bool {
    fn walk(part: &Value) -> bool {
        if part
            .get("filename")
            .and_then(Value::as_str)
            .is_some_and(|f| !f.is_empty())
        {
            return true;
        }
        part.get("parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(walk))
    }
    value.get("payload").is_some_and(walk)
}

/// The bracket-stripped `Message-Id`, or `None` when absent/empty.
fn message_id_header(raw: Option<&str>) -> Result<Option<MessageIdHeader>, GoogleError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim().trim_start_matches('<').trim_end_matches('>');
    if trimmed.is_empty() {
        return Ok(None);
    }
    MessageIdHeader::new(trimmed)
        .map(Some)
        .map_err(|e| GoogleError::protocol(format!("bad Message-Id {raw:?}: {e}")))
}

/// Pragmatically parses an RFC 5322 address-list header (`Name <a@x>, b@y`) into
/// [`EmailAddress`]es. Splits on commas outside `"…"`/`<…>`, then extracts the
/// angle-addr (or the bare token) and an optional display name. `None`/empty → no
/// addresses.
fn parse_addresses(raw: Option<&str>) -> Vec<EmailAddress> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    split_addresses(raw)
        .into_iter()
        .filter_map(|part| parse_one_address(&part))
        .collect()
}

/// Splits an address list on top-level commas (not inside quotes or angle brackets).
fn split_addresses(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut in_quotes, mut in_angle) = (false, false);
    for ch in raw.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => in_angle = true,
            '>' if !in_quotes => in_angle = false,
            ',' if !in_quotes && !in_angle => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// Parses one address token into an [`EmailAddress`]; `None` if it has no address.
fn parse_one_address(part: &str) -> Option<EmailAddress> {
    let part = part.trim();
    if part.is_empty() {
        return None;
    }
    if let (Some(open), Some(close)) = (part.find('<'), part.rfind('>'))
        && open < close
    {
        let email = part[open + 1..close].trim();
        if email.is_empty() {
            return None;
        }
        let name = part[..open].trim().trim_matches('"').trim();
        return Some(if name.is_empty() {
            EmailAddress::new(email)
        } else {
            EmailAddress::named(name, email)
        });
    }
    // A bare addr-spec with no display name.
    (!part.is_empty()).then(|| EmailAddress::new(part))
}

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;
