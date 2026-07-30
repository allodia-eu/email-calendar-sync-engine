//! Scoped folder listing and shared-store discovery, driven over a scripted mock stream.
//!
//! The `LIST` transcript below is alice's **real** one, verbatim: nine folders of her own
//! interleaved with eight belonging to `support@` and `bob@`. That interleaving is the whole
//! problem — a provider bound to one store must return only that store's folders — so it is
//! reproduced rather than simplified.
//!
//! The mock answers canned bytes whatever it is sent, so it cannot prove a request is
//! *acceptable*; that is what `tests/live_shared.rs` does. What it does prove is the request
//! **shape** (asserted from the recorded writes) and the attribution logic.

use super::*;
use crate::{
    mock::{MockStream, script, written},
    namespace::parse_namespace,
};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] Stalwart ready\r\n";

/// Alice's `LIST "" "*"`, exactly as Stalwart answered it.
const FULL_LIST: &str = "* LIST () \"/\" \"Archive\"\r\n\
     * LIST (\\Trash) \"/\" \"Deleted Items\"\r\n\
     * LIST (\\Drafts) \"/\" \"Drafts\"\r\n\
     * LIST () \"/\" \"INBOX\"\r\n\
     * LIST (\\Junk) \"/\" \"Junk Mail\"\r\n\
     * LIST () \"/\" \"Projects\"\r\n\
     * LIST (\\Sent) \"/\" \"Sent Items\"\r\n\
     * LIST (\\NoSelect) \"/\" \"Shared Folders\"\r\n\
     * LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n\
     * LIST (\\Sent) \"/\" \"Shared Folders/support@test.local/Sent Items\"\r\n\
     * LIST () \"/\" \"Shared Folders/support@test.local/INBOX\"\r\n\
     * LIST (\\NoSelect) \"/\" \"Shared Folders/bob@test.local\"\r\n\
     * LIST () \"/\" \"Shared Folders/bob@test.local/INBOX\"\r\n\
     a1 OK LIST completed\r\n";

fn alice_namespaces() -> Namespaces {
    parse_namespace(&[br#"NAMESPACE (("" "/")) (("Shared Folders" "/")) NIL"#.to_vec()])
}

/// A greeted connection over `server_script`, with `ACL` advertised or not.
async fn connection(
    server_script: Vec<u8>,
    acl: bool,
) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(server_script);
    let mut conn = Connection::open(stream).await.expect("greeting");
    conn.acl_advertised = acl;
    (conn, recorded)
}

fn names(mailboxes: &[Mailbox]) -> Vec<&str> {
    mailboxes.iter().map(|m| m.name.as_str()).collect()
}

#[tokio::test]
async fn the_personal_store_lists_only_the_credentials_own_folders() {
    // No ACL advertised, so no `MYRIGHTS` is issued and the transcript is just the LIST.
    let (mut conn, recorded) = connection(script(&[GREETING, FULL_LIST]), false).await;
    let ns = alice_namespaces();
    let store = MailStore::resolve(&ns, "INBOX");

    let mailboxes = list_store(&mut conn, &ns, &store).await.expect("lists");
    assert_eq!(
        names(&mailboxes),
        [
            "Archive",
            "Deleted Items",
            "Drafts",
            "INBOX",
            "Junk Mail",
            "Projects",
            "Sent Items",
        ],
        "the shared rows must not appear in the credential's own folder list"
    );
    // With no way to ask, every folder reports owner rights — what a caller assumed before
    // rights existed, and correct for one's own mail on a server without ACLs.
    assert!(mailboxes.iter().all(|m| m.access == MailboxAccess::owner()));

    // The request shape: one LIST, quoted, and no MYRIGHTS at all.
    let sent = written(&recorded);
    assert!(sent.contains("a1 LIST \"\" \"*\""), "{sent}");
    assert!(!sent.contains("MYRIGHTS"), "{sent}");
}

#[tokio::test]
async fn a_shared_store_lists_only_that_principals_folders_with_their_rights() {
    // The narrowed LIST, then one MYRIGHTS per *selectable* folder. The `\NoSelect`
    // container is skipped — Stalwart answers `NO Mailbox does not exist.` for it, which
    // this transcript reproduces for the one that is asked about out of order.
    let (mut conn, recorded) = connection(
        script(&[
            GREETING,
            "* LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n\
             * LIST () \"/\" \"Shared Folders/support@test.local/INBOX\"\r\n\
             * LIST (\\Sent) \"/\" \"Shared Folders/support@test.local/Sent Items\"\r\n\
             a1 OK LIST completed\r\n",
            "* MYRIGHTS \"Shared Folders/support@test.local/INBOX\" rliteswkxpa\r\n\
             a2 OK MYRIGHTS completed\r\n",
            "* MYRIGHTS \"Shared Folders/support@test.local/Sent Items\" lr\r\n\
             a3 OK MYRIGHTS completed\r\n",
        ]),
        true,
    )
    .await;
    let ns = alice_namespaces();
    let store = MailStore::resolve(&ns, "Shared Folders/support@test.local/INBOX");

    let mailboxes = list_store(&mut conn, &ns, &store).await.expect("lists");
    assert_eq!(
        names(&mailboxes),
        [
            "Shared Folders/support@test.local",
            "Shared Folders/support@test.local/INBOX",
            "Shared Folders/support@test.local/Sent Items",
        ]
    );
    // Rights land on the folder they were asked for — the whole point of the per-folder
    // round trip, and the reason they are not hoisted to the account.
    assert_eq!(mailboxes[1].access, MailboxAccess::owner());
    assert_eq!(mailboxes[2].access, MailboxAccess::reader());
    // The container has none to report, so it keeps the default rather than inheriting a
    // sibling's.
    assert_eq!(mailboxes[0].access, MailboxAccess::owner());

    let sent = written(&recorded);
    // The LIST is narrowed to the store, and the space in the prefix is inside the quotes
    // (unquoted it would be two arguments and a `BAD`).
    assert!(
        sent.contains("a1 LIST \"\" \"Shared Folders/support@test.local*\""),
        "{sent}"
    );
    // One MYRIGHTS per selectable folder, and none for the `\NoSelect` container.
    assert!(
        sent.contains("a2 MYRIGHTS \"Shared Folders/support@test.local/INBOX\""),
        "{sent}"
    );
    assert_eq!(sent.matches("MYRIGHTS").count(), 2, "{sent}");
}

#[tokio::test]
async fn a_refused_rights_lookup_leaves_the_folder_readable() {
    // `NO` is "cannot answer", not "no rights": reporting the folder unreadable would hide
    // mail the caller can plainly see.
    let (mut conn, _) = connection(
        script(&[
            GREETING,
            "* LIST () \"/\" \"INBOX\"\r\na1 OK LIST completed\r\n",
            "a2 NO Mailbox does not exist.\r\n",
        ]),
        true,
    )
    .await;
    let ns = alice_namespaces();
    let store = MailStore::resolve(&ns, "INBOX");
    let mailboxes = list_store(&mut conn, &ns, &store).await.expect("lists");
    assert_eq!(mailboxes[0].access, MailboxAccess::owner());
}

#[tokio::test]
async fn discovery_names_one_store_per_principal() {
    let (mut conn, recorded) = connection(
        script(&[
            GREETING,
            "* LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n\
             * LIST (\\NoSelect) \"/\" \"Shared Folders/bob@test.local\"\r\n\
             a1 OK LIST completed\r\n",
        ]),
        true,
    )
    .await;
    let ns = alice_namespaces();

    let stores = list_shared(&mut conn, &ns).await.expect("lists");
    let addresses: Vec<_> = stores
        .iter()
        .map(|s| s.address.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(addresses, ["support@test.local", "bob@test.local"]);
    // The handle is the full path — what a host hands back to bind a provider — not the
    // bare address, which would not name a mailbox.
    assert_eq!(
        stores[0].handle.as_str(),
        "Shared Folders/support@test.local"
    );
    // None of them is the credential's own store: IMAP's personal prefix is the empty
    // string, which is no handle at all.
    assert!(stores.iter().all(|s| !s.personal));

    // One `%` LIST per foreign namespace: one level, which is where the owner sits.
    let sent = written(&recorded);
    assert!(sent.contains("a1 LIST \"\" \"Shared Folders/%\""), "{sent}");
}

#[tokio::test]
async fn discovery_ignores_the_namespace_container_and_anything_deeper() {
    // Two things a server may add to a `%` listing: the prefix itself (which names the
    // namespace, not a store) and — if it ignores `%` — folders below the owner level.
    // Either read as a "store" would invent principals that do not exist.
    let (mut conn, _) = connection(
        script(&[
            GREETING,
            "* LIST (\\NoSelect) \"/\" \"Shared Folders\"\r\n\
             * LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n\
             * LIST () \"/\" \"Shared Folders/support@test.local/INBOX\"\r\n\
             a1 OK LIST completed\r\n",
        ]),
        true,
    )
    .await;
    let stores = list_shared(&mut conn, &alice_namespaces())
        .await
        .expect("lists");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].address.as_deref(), Some("support@test.local"));
}

#[tokio::test]
async fn resolving_matches_the_owner_component_case_insensitively() {
    let listing = "* LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n\
         a1 OK LIST completed\r\n";
    let (mut conn, _) = connection(script(&[GREETING, listing]), true).await;
    let ns = alice_namespaces();
    let resolved = resolve_shared(&mut conn, &ns, "SUPPORT@TEST.LOCAL")
        .await
        .expect("resolves");
    assert_eq!(
        resolved.handle.as_str(),
        "Shared Folders/support@test.local"
    );

    // An address no foreign namespace holds is terminal: a server lists in these namespaces
    // exactly what was granted, so absent means absent rather than withheld.
    let (mut conn, _) = connection(script(&[GREETING, listing]), true).await;
    let err = resolve_shared(&mut conn, &ns, "nobody@test.local")
        .await
        .unwrap_err();
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
}

#[tokio::test]
async fn a_provider_bound_to_a_shared_mailbox_syncs_only_that_store() {
    use engine_core::{ids::MailboxId, sync::SyncUpdate};
    use engine_provider::{Provider, SharedMailboxes};

    // Through the neutral trait surface this time: the provider is bound to a folder in the
    // shared namespace, and `sync_mailboxes` must return that principal's folders alone.
    let (stream, recorded) = MockStream::new(script(&[
        GREETING,
        "* LIST () \"/\" \"Shared Folders/support@test.local/INBOX\"\r\n\
         a1 OK LIST completed\r\n",
        "* MYRIGHTS \"Shared Folders/support@test.local/INBOX\" lr\r\n\
         a2 OK MYRIGHTS completed\r\n",
    ]));
    let mut conn = Connection::open(stream).await.expect("greeting");
    conn.acl_advertised = true;
    conn.namespace_advertised = true;
    let provider = crate::ImapProvider::with_connection_in_namespaces(
        conn,
        MailboxId::try_from("Shared Folders/support@test.local/INBOX").unwrap(),
        alice_namespaces(),
    );

    // The credential can enumerate, because a foreign namespace was advertised.
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );

    let account = engine_core::ids::AccountId::try_from("shared").unwrap();
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("folder sync")
        .update
    else {
        panic!("a LIST is always a full snapshot");
    };
    assert_eq!(names(&objects), ["Shared Folders/support@test.local/INBOX"]);
    // Read-only, so a host knows not to offer a write here even though the *provider*
    // advertises `mail_writes` (every IMAP session can issue a `UID STORE`; whether this
    // folder accepts one is the folder's business).
    assert_eq!(objects[0].access, MailboxAccess::reader());
    assert!(provider.connection_info().capabilities.mail_writes());

    let sent = written(&recorded);
    assert!(
        sent.contains("a1 LIST \"\" \"Shared Folders/support@test.local*\""),
        "{sent}"
    );
}

#[tokio::test]
async fn a_credential_with_no_shares_cannot_enumerate() {
    use engine_core::ids::MailboxId;
    use engine_provider::{Provider, SharedMailboxes};

    // `NAMESPACE` advertised but no foreign namespace in the answer (bob's case). Claiming
    // `Enumerable` would promise a list that is always empty, which a host cannot tell
    // apart from "no shares yet".
    let (stream, _) = MockStream::new(script(&[GREETING]));
    let mut conn = Connection::open(stream).await.expect("greeting");
    conn.namespace_advertised = true;
    let provider = crate::ImapProvider::with_connection_in_namespaces(
        conn,
        MailboxId::try_from("INBOX").unwrap(),
        parse_namespace(&[br#"NAMESPACE (("" "/")) NIL NIL"#.to_vec()]),
    );
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    // The verb still answers — with nothing, and without issuing a single `LIST`, because
    // there is no foreign namespace to list under. (The mock's script is exhausted after
    // the greeting, so a stray command here would fail rather than pass quietly.)
    assert!(provider.list_shared_mailboxes().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_server_without_namespaces_lists_everything_as_its_own() {
    // The pre-shared-mailbox behaviour, preserved exactly: no `NAMESPACE`, so nothing is
    // foreign and the personal store is the whole tree.
    let (mut conn, _) = connection(script(&[GREETING, FULL_LIST]), false).await;
    let ns = Namespaces::default();
    let store = MailStore::resolve(&ns, "INBOX");
    let mailboxes = list_store(&mut conn, &ns, &store).await.expect("lists");
    assert_eq!(mailboxes.len(), 13, "{:?}", names(&mailboxes));
}
