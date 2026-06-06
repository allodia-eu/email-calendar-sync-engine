//! Conformance tests for the mail model and its core invariants.

use engine_core::attachment::{Attachment, AttachmentMeta, ItemKind};
use engine_core::ids::{
    BlobId, IdError, MailboxId, MessageId, MessageIdHeader, PartId, ThreadId, Uid,
};
use engine_core::mail::{
    EmailAddress, EmailAddressGroup, EmailBodyPart, EmailHeader, Envelope, Keyword, KeywordError,
    Mailbox, MailboxRole, Message, SystemKeyword, Thread, ThreadProvenance,
};
use engine_core::membership::Memberships;

fn mailbox(id: &str) -> MailboxId {
    MailboxId::try_from(id).unwrap()
}

fn message(id: &str, mailboxes: Memberships<MailboxId>) -> Message {
    Message::new(MessageId::try_from(id).unwrap(), mailboxes)
}

#[test]
fn jmap_email_has_multiple_mailbox_memberships() {
    // One JMAP/Gmail object lives in several mailboxes/labels at once.
    let mut msg = message(
        "email-1",
        Memberships::new([mailbox("inbox"), mailbox("important")]).unwrap(),
    );
    msg.thread_id = Some(ThreadId::try_from("t-1").unwrap());
    assert_eq!(msg.mailboxes.len().get(), 2);
    assert!(msg.mailboxes.contains(&mailbox("inbox")));
    assert!(msg.mailboxes.contains(&mailbox("important")));
    assert_eq!(msg.mailboxes.iter().count(), 2);
    assert_eq!(msg.mailboxes.as_set().len(), 2);
}

#[test]
fn imap_copies_in_different_folders_are_distinct_objects() {
    // The same RFC 5322 message copied to two IMAP folders becomes two distinct
    // provider objects, each with a single membership — never coalesced.
    let shared_header = MessageIdHeader::new("shared@example.com").unwrap();

    let mut inbox_copy = message("inbox/uid/42", Memberships::of_one(mailbox("inbox")));
    inbox_copy.envelope.message_id = vec![shared_header.clone()];

    let mut archive_copy = message("archive/uid/7", Memberships::of_one(mailbox("archive")));
    archive_copy.envelope.message_id = vec![shared_header.clone()];

    assert_ne!(inbox_copy.id, archive_copy.id);
    assert_eq!(inbox_copy.mailboxes.len().get(), 1);
    assert_eq!(archive_copy.mailboxes.len().get(), 1);
    // Same Message-ID header is only a hint, not identity.
    assert_eq!(
        inbox_copy.envelope.message_id,
        archive_copy.envelope.message_id
    );
}

#[test]
fn duplicate_and_missing_message_id_are_valid() {
    let mut dup = message("m1", Memberships::of_one(mailbox("inbox")));
    let header = MessageIdHeader::new("dup@example.com").unwrap();
    dup.envelope.message_id = vec![header.clone(), header.clone()];
    assert_eq!(dup.envelope.message_id.len(), 2);
    assert_eq!(header.as_str(), "dup@example.com");
    assert_eq!(header.to_string(), "dup@example.com");

    let missing = message("m2", Memberships::of_one(mailbox("inbox")));
    assert!(missing.envelope.message_id.is_empty());
}

#[test]
fn message_unread_and_draft_semantics() {
    let mut msg = message("m1", Memberships::of_one(mailbox("inbox")));
    assert!(msg.is_unread());
    let project = Keyword::new("project-x").unwrap();
    msg.keywords.insert(project.clone());
    assert!(msg.has_keyword(&project));
    assert!(!msg.has_system_keyword(SystemKeyword::Seen));
    msg.keywords.insert(Keyword::system(SystemKeyword::Seen));
    assert!(!msg.is_unread());
}

#[test]
fn every_mailbox_role_maps_consistently() {
    let cases = [
        ("inbox", "\\Inbox", MailboxRole::Inbox),
        ("archive", "\\Archive", MailboxRole::Archive),
        ("drafts", "\\Drafts", MailboxRole::Drafts),
        ("sent", "\\Sent", MailboxRole::Sent),
        ("trash", "\\Trash", MailboxRole::Trash),
        ("junk", "\\Junk", MailboxRole::Junk),
        ("all", "\\All", MailboxRole::All),
        ("flagged", "\\Flagged", MailboxRole::Flagged),
        ("important", "\\Important", MailboxRole::Important),
    ];
    for (jmap, imap, expected) in &cases {
        assert_eq!(MailboxRole::from_jmap_role(jmap), *expected);
        assert_eq!(expected.as_jmap_role(), *jmap);
        assert_eq!(expected.to_string(), *jmap);
        // IMAP SPECIAL-USE maps to the same normalized roles (INBOX is
        // name-based, so it is not produced from a SPECIAL-USE attribute).
        if *jmap != "inbox" {
            assert_eq!(MailboxRole::from_imap_special_use(imap), *expected);
        }
    }
    // Unknown roles are preserved on both mappers.
    assert_eq!(
        MailboxRole::from_imap_special_use("\\Snoozed"),
        MailboxRole::Other("snoozed".into())
    );
}

#[test]
fn all_system_keywords_have_canonical_spelling() {
    let all = [
        SystemKeyword::Draft,
        SystemKeyword::Seen,
        SystemKeyword::Flagged,
        SystemKeyword::Answered,
        SystemKeyword::Forwarded,
        SystemKeyword::Junk,
        SystemKeyword::NotJunk,
        SystemKeyword::Phishing,
        SystemKeyword::MdnSent,
    ];
    for sk in all {
        let kw = Keyword::system(sk);
        assert_eq!(kw.as_str(), sk.as_str());
        assert_eq!(kw.to_string(), sk.as_str());
        assert_eq!(kw.as_system(), Some(sk));
    }
    assert!("$flagged".parse::<Keyword>().unwrap().as_system().is_some());
}

#[test]
fn keyword_errors_render() {
    assert!(Keyword::new("").unwrap_err().to_string().contains("empty"));
    assert!(matches!(
        Keyword::new("a".repeat(256)),
        Err(KeywordError::TooLong { actual: 256 })
    ));
    assert!(
        Keyword::new("a b")
            .unwrap_err()
            .to_string()
            .contains("must not contain")
    );
}

#[test]
fn id_errors_render_and_content_ids_bound_length() {
    assert!(
        MailboxId::try_from("")
            .unwrap_err()
            .to_string()
            .contains("empty")
    );
    let too_long = Uid::new("u".repeat(Uid::MAX_OCTETS + 1)).unwrap_err();
    assert!(matches!(too_long, IdError::TooLong { .. }));
    assert!(too_long.to_string().contains("too long"));
    // Content ids reject empty values too.
    assert_eq!(Uid::new(""), Err(IdError::Empty));
    assert_eq!(MessageIdHeader::new(""), Err(IdError::Empty));
    // The object-id `key()` accessor returns the underlying provider key.
    let id = MessageId::try_from("m-1").unwrap();
    assert_eq!(id.key().as_str(), "m-1");
}

#[test]
fn raw_payload_accessors() {
    use engine_core::raw::{RawIcal, RawJsCalendar, RawMime};
    assert_eq!(RawIcal::new("BEGIN:VCALENDAR").len(), 15);
    assert!(!RawIcal::new("x").is_empty());
    assert_eq!(RawJsCalendar::new("{}").len(), 2);
    assert!(!RawMime::new(b"abc".to_vec()).is_empty());
}

#[test]
fn attachment_kinds_expose_metadata_blob_and_bytes() {
    let blob = BlobId::try_from("b1").unwrap();
    let file = Attachment::File {
        meta: AttachmentMeta {
            name: Some("a.pdf".into()),
            media_type: Some("application/pdf".into()),
            size: Some(10),
        },
        blob: Some(blob.clone()),
    };
    assert!(file.has_bytes());
    assert_eq!(file.blob(), Some(&blob));
    assert_eq!(file.meta().name.as_deref(), Some("a.pdf"));

    let inline = Attachment::Inline {
        meta: AttachmentMeta::default(),
        cid: "cid-1".into(),
        blob: Some(blob.clone()),
    };
    assert!(inline.has_bytes());
    assert_eq!(inline.blob(), Some(&blob));
    assert!(inline.meta().size.is_none());

    let item = Attachment::Item {
        meta: AttachmentMeta::default(),
        item: ItemKind::Contact,
        blob: None,
    };
    assert!(item.has_bytes());
    assert!(item.blob().is_none());
    assert!(item.meta().name.is_none());

    let reference = Attachment::Reference {
        meta: AttachmentMeta::default(),
        uri: "https://x/y".into(),
    };
    assert!(!reference.has_bytes());
    assert!(reference.blob().is_none());
}

#[test]
fn body_structure_leaf_and_container() {
    let leaf = EmailBodyPart::leaf(
        PartId::try_from("1").unwrap(),
        BlobId::try_from("b1").unwrap(),
        "text/plain",
        12,
    );
    assert!(!leaf.is_multipart());
    let mut container = EmailBodyPart::multipart("multipart/mixed", vec![leaf]);
    container
        .headers
        .push(EmailHeader::new("Content-Type", "multipart/mixed"));
    assert!(container.is_multipart());
}

#[test]
fn thread_and_address_group_and_mailbox() {
    let thread = Thread::new(
        ThreadId::try_from("t1").unwrap(),
        ThreadProvenance::ProviderAssigned,
        vec![MessageId::try_from("m1").unwrap()],
    );
    assert_eq!(thread.provenance, ThreadProvenance::ProviderAssigned);

    let group = EmailAddressGroup {
        name: Some("Team".into()),
        addresses: vec![EmailAddress::named("A", "a@x"), EmailAddress::new("b@x")],
    };
    assert_eq!(group.addresses.len(), 2);

    let env = Envelope {
        from: vec![EmailAddress::new("sender@x")],
        ..Envelope::default()
    };
    assert_eq!(env.from.len(), 1);

    let mut mb = Mailbox::new(mailbox("inbox"), "Inbox");
    mb.role = Some(MailboxRole::Inbox);
    assert_eq!(mb.role, Some(MailboxRole::Inbox));
}
