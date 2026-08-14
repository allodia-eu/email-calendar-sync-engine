//! Projecting a [`Message`] into its search-index rows.

use serde::{Deserialize, Serialize};

use super::{FtsField, FtsRow, MembershipKind, MembershipRow, normalize_addr};
use crate::{
    ids::{MessageIdHeader, ProviderKey, ThreadId},
    mail::{EmailAddress, MailFlags, MailKeywordChange, Message},
    time::UtcDateTime,
};

/// Which address header an address-junction row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AddressField {
    /// The `From` header.
    From,
    /// The `To` header.
    To,
    /// The `Cc` header.
    Cc,
}

/// An address-junction row (the `mail_address` table): one `field` address of one
/// message. `addr` is normalized (trimmed, lowercased) for case-insensitive
/// matching; `name` preserves the original display name for results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAddressRow {
    /// The message this address belongs to.
    pub key: ProviderKey,
    /// Which header the address came from.
    pub field: AddressField,
    /// The normalized address.
    pub addr: String,
    /// The display name, if the header carried one.
    pub name: Option<String>,
}

/// The stored row for one mail object (the `message` table): everything a list row, a sort, a
/// thread group and a date filter need, and nothing that has to be parsed out of a payload.
///
/// This is the *whole* of what a mailbox list reads. A store answers a windowed list from these
/// columns alone, so the cost of the first page is the size of the page rather than the size of
/// the mailbox. Opening a message still reads its normalized object; showing it in a list does
/// not.
///
/// `date_utc` is the message's `received_at`, falling back to `sent_at` (the JMAP `Email/query`
/// convention), and `None` when neither is known — the executor excludes such a message from
/// `before:`/`after:` filtering, and a list sinks it below every dated message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailRow {
    /// The message.
    pub key: ProviderKey,
    /// The thread the message belongs to, if threading is resolved.
    pub thread_id: Option<ThreadId>,
    /// The first `Message-ID` header value, which collapses one message's copies across folders.
    pub message_id: Option<MessageIdHeader>,
    /// The date used for ordering and for `before:`/`after:`.
    pub date_utc: Option<UtcDateTime>,
    /// The system keywords the row's appearance depends on.
    pub flags: MailFlags,
    /// Whether the message has a non-inline attachment.
    pub has_attachment: bool,
    /// The first sender's display name, if the header carried one.
    pub from_name: Option<String>,
    /// The first sender's address, as the header spelled it.
    pub from_addr: Option<String>,
    /// The `Subject`, if present.
    pub subject: Option<String>,
    /// The list snippet (JMAP `preview`).
    pub preview: Option<String>,
}

/// The rows a [`MailKeywordChange`] rewrites: the `message` row's `flags` column, and the
/// message's `keyword`-kind memberships.
///
/// Deliberately not a [`MailRow`]: a keyword change carries no subject, no sender and no date,
/// so a whole-row upsert built from one would blank every column the provider did not send.
/// This names the two things that moved, and a store writes exactly those.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailKeywordRow {
    /// The message whose keywords moved.
    pub key: ProviderKey,
    /// The system keywords, as the bitfield the `message` row sorts and filters on.
    pub flags: MailFlags,
    /// The complete keyword set, as the membership values `keyword:` searches. Replaces the
    /// message's existing keyword memberships; its mailbox memberships are left alone.
    pub keywords: Vec<String>,
}

/// The row a thread assignment rewrites: the `message` row's `thread_id` column, alone.
///
/// The engine derives a thread id from the reference graph, so it is the engine's to write and
/// no provider's to send. Writing it as a whole-row upsert — re-projected from a stored payload
/// — would carry every *other* column along with it, including the flags a keyword change had
/// just moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailThreadRow {
    /// The message being assigned.
    pub key: ProviderKey,
    /// The thread it now belongs to.
    pub thread_id: ThreadId,
}

/// Projects a [`MailKeywordChange`] into the two things a keyword-only write touches.
#[must_use]
pub fn project_keyword_change(change: &MailKeywordChange) -> MailKeywordRow {
    MailKeywordRow {
        key: change.key.clone(),
        flags: MailFlags::from_keywords(&change.keywords),
        keywords: change
            .keywords
            .iter()
            .map(|keyword| keyword.as_str().to_owned())
            .collect(),
    }
}

/// All search-index rows derived from one mail object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailProjection {
    /// The full-text document (`subject`, `body`).
    pub fts: FtsRow,
    /// The stored message row.
    pub row: MailRow,
    /// The `from`/`to`/`cc` address-junction rows.
    pub addresses: Vec<MailAddressRow>,
    /// The mailbox and keyword membership rows.
    pub memberships: Vec<MembershipRow>,
}

/// Projects a normalized [`Message`] into its search-index rows.
///
/// Text projection is deliberately basic here — `subject` plus the available
/// preview/reply text and the sender/recipient address text — because full
/// MIME/HTML extraction and chunking belong to a later `engine-index` step
/// (`north-star.md` workspace shape). The address text is folded into `body` so an
/// unscoped search term matches sender/recipient identity (search.md). The
/// structured rows are complete: every `from`/`to`/`cc` address, mailbox
/// membership, and keyword the message carries.
#[must_use]
pub fn project_message(message: &Message) -> MailProjection {
    let key = message.id.key().clone();

    let mut fields = Vec::new();
    if let Some(subject) = &message.envelope.subject
        && !subject.is_empty()
    {
        fields.push(FtsField::new("subject", subject));
    }
    // The body folds together the preview/reply text *and* the sender/recipient
    // address text, so a bare (unscoped) search-box term matches an address even
    // when the body is empty (metadata-tier sync). The structured `mail_address`
    // rows below still back the exact `from:`/`to:`/`cc:` filters.
    let body = join_nonempty([body_text(message), address_text(message)]);
    if !body.is_empty() {
        fields.push(FtsField::new("body", body));
    }

    let mut addresses = Vec::new();
    push_addresses(
        &mut addresses,
        &key,
        AddressField::From,
        &message.envelope.from,
    );
    push_addresses(&mut addresses, &key, AddressField::To, &message.envelope.to);
    push_addresses(&mut addresses, &key, AddressField::Cc, &message.envelope.cc);

    let mut memberships = Vec::new();
    for mailbox in message.mailboxes.iter() {
        memberships.push(MembershipRow {
            key: key.clone(),
            kind: MembershipKind::Mailbox,
            value: mailbox.as_str().to_owned(),
        });
    }
    for keyword in &message.keywords {
        memberships.push(MembershipRow {
            key: key.clone(),
            kind: MembershipKind::Keyword,
            value: keyword.as_str().to_owned(),
        });
    }

    let sender = message.envelope.from.first();
    MailProjection {
        fts: FtsRow::new(key.clone(), fields),
        row: MailRow {
            key: key.clone(),
            thread_id: message.thread_id().cloned(),
            message_id: message.envelope.message_id.first().cloned(),
            date_utc: message.received_at.or(message.sent_at),
            flags: MailFlags::from_keywords(&message.keywords),
            has_attachment: message.has_attachment,
            from_name: sender.and_then(|address| address.name.clone()),
            from_addr: sender.map(|address| address.email.as_str().to_owned()),
            subject: message.envelope.subject.clone(),
            preview: message.preview.clone(),
        },
        addresses,
        memberships,
    }
}

/// The basic searchable body text: the preview plus any reply-unique text.
fn body_text(message: &Message) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(preview) = &message.preview {
        parts.push(preview);
    }
    if let Some(reply) = &message.reply_unique_text {
        parts.push(reply);
    }
    parts.join(" ")
}

/// The searchable address text: every `from`/`to`/`cc` address's email and display
/// name, space-joined. Folded into the FTS `body` so an unscoped term matches
/// sender/recipient identity; the FTS tokenizer splits and case-folds, so a typed
/// `allodia` (or the prefix `allo`) matches the address `info@allodia.eu`.
fn address_text(message: &Message) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for list in [
        &message.envelope.from,
        &message.envelope.to,
        &message.envelope.cc,
    ] {
        for address in list {
            let email = address.email.trim();
            if !email.is_empty() {
                parts.push(email);
            }
            if let Some(name) = address.name.as_deref() {
                let name = name.trim();
                if !name.is_empty() {
                    parts.push(name);
                }
            }
        }
    }
    parts.join(" ")
}

/// Joins the non-empty segments with a single space, so an empty body or empty
/// address set never leaves a leading/trailing or doubled space.
fn join_nonempty<const N: usize>(parts: [String; N]) -> String {
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Appends a normalized junction row for each non-empty address.
fn push_addresses(
    out: &mut Vec<MailAddressRow>,
    key: &ProviderKey,
    field: AddressField,
    addresses: &[EmailAddress],
) {
    for address in addresses {
        let addr = normalize_addr(&address.email);
        if addr.is_empty() {
            continue;
        }
        out.push(MailAddressRow {
            key: key.clone(),
            field,
            addr,
            name: address.name.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::{MailboxId, MessageId},
        mail::{Keyword, SystemKeyword},
        membership::Memberships,
    };

    fn message() -> Message {
        Message::new(
            MessageId::try_from("m1").unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        )
    }

    #[test]
    fn keyword_change_projects_the_bitfield_and_the_membership_values() {
        let change = MailKeywordChange::new(
            ProviderKey::new("m1").unwrap(),
            [
                Keyword::system(SystemKeyword::Seen),
                Keyword::new("todo").unwrap(),
            ]
            .into_iter()
            .collect(),
        );
        let row = project_keyword_change(&change);
        assert_eq!(row.key.as_str(), "m1");
        assert!(row.flags.seen());
        assert!(!row.flags.flagged());
        // The user keyword reaches the membership values but not the bitfield, which
        // carries only the system keywords a list row's appearance depends on.
        assert_eq!(row.keywords, vec!["$seen".to_owned(), "todo".to_owned()]);
    }

    #[test]
    fn clearing_every_keyword_projects_an_empty_row_rather_than_nothing() {
        // Marking a read message unread empties the set. The row must still be produced —
        // a store that skipped it would leave the message `$seen` forever.
        let row = project_keyword_change(&MailKeywordChange::new(
            ProviderKey::new("m1").unwrap(),
            std::collections::BTreeSet::new(),
        ));
        assert_eq!(row.flags.bits(), 0);
        assert!(row.keywords.is_empty());
    }

    #[test]
    fn projects_addresses_subject_and_membership() {
        let mut msg = message();
        msg.envelope.subject = Some("Quarterly Report".into());
        msg.envelope.from = vec![EmailAddress::named("Alice", "Alice@Example.com")];
        msg.envelope.to = vec![
            EmailAddress::new("bob@example.com"),
            EmailAddress::new("  "), // whitespace-only is dropped
        ];
        msg.preview = Some("see attached".into());
        msg.has_attachment = true;
        msg.keywords.insert(Keyword::system(SystemKeyword::Flagged));

        let p = project_message(&msg);

        // FTS: subject + body. The body folds the preview together with the
        // address text (email + display name), so an unscoped term matches an
        // address. The blank `to` contributes nothing.
        assert_eq!(
            p.fts.fields,
            vec![
                FtsField::new("subject", "Quarterly Report"),
                FtsField::new(
                    "body",
                    "see attached Alice@Example.com Alice bob@example.com"
                ),
            ]
        );
        // Addresses: from normalized + lowercased, name kept; the blank `to` dropped.
        assert_eq!(p.addresses.len(), 2);
        let from = &p.addresses[0];
        assert_eq!(from.field, AddressField::From);
        assert_eq!(from.addr, "alice@example.com");
        assert_eq!(from.name.as_deref(), Some("Alice"));
        assert_eq!(p.addresses[1].field, AddressField::To);
        assert_eq!(p.addresses[1].addr, "bob@example.com");
        // Membership: the inbox mailbox + the $flagged keyword.
        assert!(p.memberships.contains(&MembershipRow {
            key: p.fts.key.clone(),
            kind: MembershipKind::Mailbox,
            value: "inbox".into(),
        }));
        assert!(p.memberships.contains(&MembershipRow {
            key: p.fts.key.clone(),
            kind: MembershipKind::Keyword,
            value: "$flagged".into(),
        }));
        // Scalars: the whole of what a list row renders, with no payload to open.
        assert!(p.row.has_attachment);
        assert_eq!(p.row.subject.as_deref(), Some("Quarterly Report"));
        assert_eq!(p.row.from_name.as_deref(), Some("Alice"));
        assert_eq!(p.row.from_addr.as_deref(), Some("Alice@Example.com"));
        assert_eq!(p.row.preview.as_deref(), Some("see attached"));
        assert!(p.row.flags.flagged());
        assert!(p.row.flags.is_unread());
    }

    #[test]
    fn the_row_carries_the_first_message_id_for_cross_folder_dedup() {
        let mut msg = message();
        msg.envelope.message_id = vec![
            MessageIdHeader::new("first@example.com").unwrap(),
            MessageIdHeader::new("second@example.com").unwrap(),
        ];
        assert_eq!(
            project_message(&msg).row.message_id.map(String::from),
            Some("first@example.com".to_owned())
        );
    }

    #[test]
    fn a_sender_without_a_display_name_still_yields_its_address() {
        let mut msg = message();
        msg.envelope.from = vec![EmailAddress::new("bob@example.com")];
        let row = project_message(&msg).row;
        assert_eq!(row.from_name, None);
        assert_eq!(row.from_addr.as_deref(), Some("bob@example.com"));
    }

    #[test]
    fn date_prefers_received_then_falls_back_to_sent() {
        let mut msg = message();
        msg.sent_at = Some("2026-01-01T00:00:00Z".parse().unwrap());
        assert_eq!(
            project_message(&msg).row.date_utc,
            Some("2026-01-01T00:00:00Z".parse().unwrap())
        );
        msg.received_at = Some("2026-02-02T00:00:00Z".parse().unwrap());
        assert_eq!(
            project_message(&msg).row.date_utc,
            Some("2026-02-02T00:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn empty_subject_and_body_produce_no_fts_fields() {
        let p = project_message(&message());
        assert!(p.fts.fields.is_empty());
        assert_eq!(p.row.date_utc, None);
        assert!(!p.row.has_attachment);
    }

    #[test]
    fn body_concatenates_preview_and_reply_text() {
        let mut msg = message();
        msg.preview = Some("preview".into());
        msg.reply_unique_text = Some("reply body".into());
        let p = project_message(&msg);
        assert_eq!(
            p.fts.fields,
            vec![FtsField::new("body", "preview reply body")]
        );
    }

    #[test]
    fn addresses_are_folded_into_the_body_without_a_preview() {
        // A metadata-tier message with no preview/reply and a subject that does
        // not mention the address still gets a body of the address text, so an
        // unscoped term can match the sender/recipient identity.
        let mut msg = message();
        msg.envelope.subject = Some("Weekly update".into());
        msg.envelope.from = vec![EmailAddress::new("info@allodia.eu")];
        let p = project_message(&msg);
        assert_eq!(
            p.fts.fields,
            vec![
                FtsField::new("subject", "Weekly update"),
                FtsField::new("body", "info@allodia.eu"),
            ]
        );
    }
}
