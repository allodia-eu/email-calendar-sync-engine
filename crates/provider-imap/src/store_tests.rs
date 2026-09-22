//! `NAMESPACE` parsing and store attribution, on the shapes real servers send.

use engine_core::ids::MailboxId;

use super::*;

fn line(text: &str) -> Vec<Vec<u8>> {
    vec![text.as_bytes().to_vec()]
}

/// Stalwart's answer for an account two stores are shared with, verbatim.
fn stalwart() -> Namespaces {
    parse_namespace(
        &line(r#"NAMESPACE (("" "/")) (("Shared Folders" "/")) NIL"#),
        false,
    )
}

fn mailbox(id: &str, parent: Option<&str>) -> Mailbox {
    let mut mailbox = Mailbox::new(MailboxId::try_from(id).unwrap(), "leaf");
    mailbox.parent = parent.map(|p| MailboxId::try_from(p).unwrap());
    mailbox
}

#[test]
fn stalwart_puts_its_shares_in_the_other_users_position() {
    let namespaces = stalwart();
    assert_eq!(
        namespaces.personal,
        [Namespace {
            prefix: String::new(),
            delimiter: Some("/".into())
        }]
    );
    assert_eq!(namespaces.other_users[0].prefix, "Shared Folders");
    assert!(namespaces.shared.is_empty());
    assert_eq!(namespaces.foreign().count(), 1);
}

#[test]
fn a_trailing_delimiter_on_the_prefix_is_normalized_off() {
    // Dovecot writes its shared prefix with the delimiter on the end.
    let namespaces = parse_namespace(
        &line(r#"NAMESPACE (("" "/")) NIL (("shared/" "/"))"#),
        false,
    );
    assert_eq!(namespaces.shared[0].prefix, "shared");
    assert_eq!(namespaces.shared[0].join("bob"), "shared/bob");
}

#[test]
fn a_rev1_prefix_is_decoded_like_a_mailbox_name() {
    // On IMAP4rev1 a prefix travels as modified UTF-7, exactly as the names under it do; it
    // has to be compared in the decoded form a mailbox id carries.
    let lines = line(r#"NAMESPACE (("" "/")) NIL (("&ANY-ffentlich/" "/"))"#);
    assert_eq!(parse_namespace(&lines, true).shared[0].prefix, "Öffentlich");
    // On rev2 the same bytes are a literal name and are left alone.
    assert_eq!(
        parse_namespace(&lines, false).shared[0].prefix,
        "&ANY-ffentlich"
    );
}

#[test]
fn a_missing_or_unreadable_answer_means_everything_is_the_credentials_own() {
    for lines in [
        vec![],
        line("NAMESPACE NIL NIL NIL"),
        line("CAPABILITY IMAP4rev1"),
        line("NAMESPACE (("),
    ] {
        let namespaces = parse_namespace(&lines, false);
        assert_eq!(namespaces.foreign().count(), 0, "{lines:?}");
        assert!(
            MailStore::own(&namespaces)
                .relative("Shared Folders/bob/INBOX")
                .is_some()
        );
    }
}

#[test]
fn other_untagged_data_before_the_answer_is_skipped() {
    let lines = vec![
        b"CAPABILITY IMAP4rev1 ACL NAMESPACE".to_vec(),
        br#"NAMESPACE (("" "/")) (("Shared Folders" "/")) NIL"#.to_vec(),
    ];
    let namespaces = parse_namespace(&lines, false);
    assert_eq!(namespaces.other_users[0].prefix, "Shared Folders");
}

#[test]
fn a_flat_namespace_has_no_delimiter_to_end_its_prefix_at() {
    // RFC 2342 §5 allows a `NIL` delimiter: a namespace with no hierarchy, whose prefix is
    // the whole marker, so everything written after it is inside.
    let namespaces = parse_namespace(
        &line(r##"NAMESPACE (("" "/")) NIL (("#news." NIL))"##),
        false,
    );
    let flat = &namespaces.shared[0];
    assert_eq!(
        (flat.prefix.as_str(), flat.delimiter.as_deref()),
        ("#news.", None)
    );
    assert_eq!(flat.relative("#news.comp.mail"), Some("comp.mail"));
    assert_eq!(flat.join("comp.mail"), "#news.comp.mail");
    assert!(
        MailStore::own(&namespaces)
            .relative("#news.comp.mail")
            .is_none()
    );
    // Foreign, but no level of it is an owner's: nothing there is a store to discover or bind.
    assert!(shared_store(flat, "#news.comp.mail").is_none());
    assert!(MailStore::shared(&namespaces, "#news.comp.mail").is_err());
    // The personal namespace's empty prefix holds every name, and adds nothing to one.
    let own = &namespaces.personal[0];
    assert_eq!(own.relative("Sent"), Some("Sent"));
    assert_eq!(own.join("Sent"), "Sent");
}

#[test]
fn a_prefix_match_must_end_at_the_delimiter() {
    let shared = &stalwart().other_users[0];
    assert_eq!(shared.relative("Shared Folders"), Some(""));
    assert_eq!(
        shared.relative("Shared Folders/bob@test.local/INBOX"),
        Some("bob@test.local/INBOX")
    );
    // Somebody's own folder that happens to start with the same letters.
    assert_eq!(shared.relative("Shared Foldersomething"), None);
    assert_eq!(shared.relative("INBOX"), None);
}

#[test]
fn the_own_store_is_everything_outside_the_foreign_namespaces() {
    let own = MailStore::own(&stalwart());
    for mine in [
        "INBOX",
        "Sent Items",
        "Archive/Nested",
        "Shared Foldersomething",
    ] {
        assert!(own.relative(mine).is_some(), "{mine}");
        assert_eq!(own.qualify(mine), mine);
    }
    for theirs in [
        "Shared Folders",
        "Shared Folders/support@test.local",
        "Shared Folders/support@test.local/Sent Items",
    ] {
        assert!(own.relative(theirs).is_none(), "{theirs}");
    }
    assert_eq!(own.list_pattern(), "*");
}

#[test]
fn a_shared_store_is_one_owner_below_a_foreign_prefix() {
    let namespaces = stalwart();
    let store = MailStore::shared(&namespaces, "Shared Folders/support@test.local").unwrap();
    assert_eq!(store.list_pattern(), "Shared Folders/support@test.local*");
    assert!(
        store
            .relative("Shared Folders/support@test.local/INBOX")
            .is_some()
    );
    assert_eq!(
        store.relative("Shared Folders/support@test.local/Sent Items"),
        Some("Sent Items")
    );
    // The root is the store's own top (whether it is a *folder* is `adopt`'s call); the
    // sibling store is someone else, and so is alice.
    assert_eq!(
        store.relative("Shared Folders/support@test.local"),
        Some("")
    );
    assert!(
        store
            .relative("Shared Folders/bob@test.local/INBOX")
            .is_none()
    );
    assert!(
        store
            .relative("Shared Folders/support@test.local.invalid/INBOX")
            .is_none()
    );
    assert!(store.relative("INBOX").is_none());
    assert_eq!(
        store.qualify("Drafts"),
        "Shared Folders/support@test.local/Drafts"
    );
}

#[test]
fn a_handle_discovery_could_not_have_produced_is_refused() {
    let namespaces = stalwart();
    for bad in [
        // The credential's own tree, the container itself, and one level too deep.
        "INBOX",
        "Shared Folders",
        "Shared Folders/support@test.local/INBOX",
    ] {
        let err = MailStore::shared(&namespaces, bad).unwrap_err();
        assert_eq!(
            err.class(),
            engine_core::error::FailureClass::InvalidState,
            "{bad}"
        );
    }
    // A server that advertises no foreign namespace shares nothing.
    assert!(MailStore::shared(&Namespaces::default(), "Shared Folders/bob").is_err());
}

#[test]
fn the_most_specific_foreign_prefix_owns_a_folder() {
    let namespaces = parse_namespace(
        &line(r#"NAMESPACE (("" "/")) (("Other" "/")) (("Other/Public" "/"))"#),
        false,
    );
    // Under `Other/Public` the owner is the next component, not `Public`.
    let store = MailStore::shared(&namespaces, "Other/Public/announcements").unwrap();
    assert!(store.relative("Other/Public/announcements/2026").is_some());
}

#[test]
fn a_shared_store_reads_like_an_account() {
    let store = MailStore::shared(&stalwart(), "Shared Folders/support@test.local").unwrap();
    let root = "Shared Folders/support@test.local";

    let inbox = store
        .adopt(mailbox(&format!("{root}/INBOX"), Some(root)), true)
        .unwrap();
    // Top level of the store: no parent, and the reserved name is the Inbox again.
    assert_eq!(inbox.parent, None);
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));

    // A folder deeper down keeps its parent, and only the top-level INBOX is the Inbox.
    let nested = store
        .adopt(
            mailbox(
                &format!("{root}/Archive/INBOX"),
                Some(&format!("{root}/Archive")),
            ),
            true,
        )
        .unwrap();
    assert_eq!(nested.parent.unwrap().as_str(), format!("{root}/Archive"));
    assert_eq!(nested.role, None);

    // A role the server already gave is not overwritten.
    let mut sent = mailbox(&format!("{root}/Sent Items"), Some(root));
    sent.role = Some(MailboxRole::Sent);
    assert_eq!(
        store.adopt(sent, true).unwrap().role,
        Some(MailboxRole::Sent)
    );

    // Another store's folder is not adopted at all.
    assert!(store.adopt(mailbox("INBOX", None), true).is_none());
}

#[test]
fn a_selectable_root_is_the_stores_inbox_and_a_container_is_nothing() {
    // Stalwart: the root is a `\Noselect` container with the INBOX below it.
    let stalwart = MailStore::shared(&stalwart(), "Shared Folders/support@test.local").unwrap();
    assert!(
        stalwart
            .adopt(
                mailbox("Shared Folders/support@test.local", Some("Shared Folders")),
                false
            )
            .is_none()
    );
    // Dovecot: the root *is* the INBOX, selectable and holding the mail, with `…/INBOX`
    // an alias it never lists. Dropping it would lose the store's Inbox — or, for a share
    // of one INBOX alone, every folder the store has.
    let dovecot = parse_namespace(
        &line(r#"NAMESPACE (("" "/")) (("shared/" "/")) NIL"#),
        false,
    );
    let store = MailStore::shared(&dovecot, "shared/bob@test.local").unwrap();
    assert_eq!(store.list_pattern(), "shared/bob@test.local*");
    let inbox = store
        .adopt(mailbox("shared/bob@test.local", Some("shared")), true)
        .unwrap();
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));
    assert_eq!(inbox.name, "INBOX");
    assert_eq!(inbox.parent, None);
    // A sibling that merely starts with the same characters is someone else's store.
    assert!(
        store
            .adopt(
                mailbox("shared/bob@test.local.invalid", Some("shared")),
                true
            )
            .is_none()
    );
}

#[test]
fn the_own_store_leaves_a_folder_as_the_server_described_it() {
    let own = MailStore::own(&stalwart());
    let archived = own
        .adopt(mailbox("Archive/Nested", Some("Archive")), true)
        .unwrap();
    assert_eq!(archived.parent.unwrap().as_str(), "Archive");
    assert!(
        own.adopt(mailbox("Shared Folders/bob@test.local/INBOX", None), true)
            .is_none()
    );
}

#[test]
fn discovery_finds_one_store_per_owner_and_nothing_else() {
    let namespace = &stalwart().other_users[0];
    let support = shared_store(namespace, "Shared Folders/support@test.local").unwrap();
    assert_eq!(support.handle.as_str(), "Shared Folders/support@test.local");
    assert_eq!(support.name, "support@test.local");
    assert_eq!(support.address.as_deref(), Some("support@test.local"));

    // The container, a folder inside a store, and a row from elsewhere are not stores.
    assert!(shared_store(namespace, "Shared Folders").is_none());
    assert!(shared_store(namespace, "Shared Folders/support@test.local/INBOX").is_none());
    assert!(shared_store(namespace, "INBOX").is_none());

    // A server keyed by bare user names gives a label and no address.
    let bare = shared_store(namespace, "Shared Folders/bob").unwrap();
    assert_eq!(bare.name, "bob");
    assert!(bare.address.is_none());
}
