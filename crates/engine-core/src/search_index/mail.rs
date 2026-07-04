//! Projecting a [`Message`] into its search-index rows.

use serde::{Deserialize, Serialize};

use super::{FtsField, FtsRow, MembershipKind, MembershipRow, normalize_addr};
use crate::ids::{ProviderKey, ThreadId};
use crate::mail::{EmailAddress, Message};
use crate::time::UtcDateTime;

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

/// The scalar index row for one mail object (the `mail_index` table).
///
/// `date_utc` is the message's `received_at`, falling back to `sent_at` (the JMAP
/// `Email/query` convention), and `None` when neither is known — the executor
/// excludes such a message from `before:`/`after:` filtering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailIndexRow {
    /// The message.
    pub key: ProviderKey,
    /// The date used for `before:`/`after:`.
    pub date_utc: Option<UtcDateTime>,
    /// Whether the message has a non-inline attachment.
    pub has_attachment: bool,
    /// The thread the message belongs to, if threading is resolved.
    pub thread_id: Option<ThreadId>,
}

/// All search-index rows derived from one mail object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailProjection {
    /// The full-text document (`subject`, `body`).
    pub fts: FtsRow,
    /// The scalar filter row.
    pub index: MailIndexRow,
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

    MailProjection {
        fts: FtsRow::new(key.clone(), fields),
        index: MailIndexRow {
            key: key.clone(),
            date_utc: message.received_at.or(message.sent_at),
            has_attachment: message.has_attachment,
            thread_id: message.thread_id.clone(),
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
    use crate::ids::{MailboxId, MessageId};
    use crate::mail::{Keyword, SystemKeyword};
    use crate::membership::Memberships;

    fn message() -> Message {
        Message::new(
            MessageId::try_from("m1").unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        )
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
        // Scalars.
        assert!(p.index.has_attachment);
    }

    #[test]
    fn date_prefers_received_then_falls_back_to_sent() {
        let mut msg = message();
        msg.sent_at = Some("2026-01-01T00:00:00Z".parse().unwrap());
        assert_eq!(
            project_message(&msg).index.date_utc,
            Some("2026-01-01T00:00:00Z".parse().unwrap())
        );
        msg.received_at = Some("2026-02-02T00:00:00Z".parse().unwrap());
        assert_eq!(
            project_message(&msg).index.date_utc,
            Some("2026-02-02T00:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn empty_subject_and_body_produce_no_fts_fields() {
        let p = project_message(&message());
        assert!(p.fts.fields.is_empty());
        assert_eq!(p.index.date_utc, None);
        assert!(!p.index.has_attachment);
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
