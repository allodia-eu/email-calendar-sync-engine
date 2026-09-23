//! One store's folder list, discovery, and store-scoped placement, driven over scripted
//! transcripts shaped like Stalwart's answers for an account two stores are shared with.
//!
//! The scripts only prove what is *parsed and decided*; `tests/live_shared.rs` proves a real
//! server accepts what is *sent*. Each test here still asserts the commands it wrote, since a
//! canned answer would pass over a wrong pattern.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, SharedMailboxId},
    mail::{MailboxAccess, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Provider, SharedMailboxes};
use futures_util::StreamExt;

use crate::{
    ImapProvider,
    capability::Negotiated,
    mock::{MockStream, Recorded, script, written},
    place::{Filing, resolve_filing_folder},
    store::{MailStore, parse_namespace},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
const NAMESPACE: &str = "* NAMESPACE ((\"\" \"/\")) ((\"Shared Folders\" \"/\")) NIL\r\n";
const SUPPORT: &str = "Shared Folders/support@test.local";
const BOB: &str = "Shared Folders/bob@test.local";

/// A logged-in session that advertised `extensions` (rev1, so names are modified UTF-7).
async fn session(extensions: &[&str], server: &[&str]) -> (Connection<MockStream>, Recorded) {
    let mut parts = vec![GREETING, LOGIN_OK];
    parts.extend_from_slice(server);
    let (stream, recorded) = MockStream::new(script(&parts));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice", "pw").await.unwrap();
    let advertised: Vec<String> = extensions.iter().map(|e| (*e).to_owned()).collect();
    connection.negotiated = Negotiated::from_capabilities(&advertised);
    (connection, recorded)
}

fn provider(connection: Connection<MockStream>, shared: Option<&str>) -> ImapProvider<MockStream> {
    let mut provider =
        ImapProvider::with_connection(connection, MailboxId::try_from("INBOX").unwrap());
    provider.shared_mailbox = shared.map(|handle| SharedMailboxId::try_from(handle).unwrap());
    provider
}

fn account() -> AccountId {
    AccountId::try_from("alice").unwrap()
}

async fn folders(provider: &ImapProvider<MockStream>) -> Vec<engine_core::mail::Mailbox> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("folder sync")
        .update
    else {
        panic!("a LIST is a snapshot");
    };
    objects
}

const FULL: &[&str] = &["NAMESPACE", "ACL", "LIST-STATUS", "SPECIAL-USE"];

#[tokio::test]
async fn the_own_folder_list_leaves_every_shared_folder_out() {
    let (connection, recorded) = session(
        FULL,
        &[
            NAMESPACE,
            "a2 OK NAMESPACE completed\r\n",
            "* LIST () \"/\" \"INBOX\"\r\n* STATUS \"INBOX\" (UNSEEN 3)\r\n",
            "* LIST (\\Sent) \"/\" \"Sent Items\"\r\n",
            "* LIST (\\NoSelect) \"/\" \"Shared Folders\"\r\n",
            "* LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n",
            "* LIST (\\Sent) \"/\" \"Shared Folders/support@test.local/Sent Items\"\r\n",
            "* LIST () \"/\" \"Shared Folders/bob@test.local/INBOX\"\r\n",
            "a3 OK LIST completed\r\n",
            "* MYRIGHTS \"INBOX\" rliteswkxpa\r\na4 OK done\r\n",
            "* MYRIGHTS \"Sent Items\" rliteswkxpa\r\na5 OK done\r\n",
        ],
    )
    .await;
    let folders = folders(&provider(connection, None)).await;
    let ids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["INBOX", "Sent Items"]);
    assert_eq!(folders[0].unread_count, Some(3));
    assert!(folders.iter().all(|f| f.access == MailboxAccess::owner()));
    // Rights were asked for the credential's own two folders, pipelined — never for anyone
    // else's, and never for a container.
    let sent = written(&recorded);
    assert!(sent.contains("a3 LIST \"\" \"*\" RETURN (SPECIAL-USE STATUS (UNSEEN))"));
    assert!(sent.ends_with("a4 MYRIGHTS \"INBOX\"\r\na5 MYRIGHTS \"Sent Items\"\r\n"));
}

#[tokio::test]
async fn a_shared_store_lists_its_own_subtree_as_an_account() {
    let (connection, recorded) = session(
        FULL,
        &[
            NAMESPACE,
            "a2 OK NAMESPACE completed\r\n",
            "* LIST () \"/\" \"Shared Folders/bob@test.local/INBOX\"\r\n",
            "* STATUS \"Shared Folders/bob@test.local/INBOX\" (UNSEEN 13)\r\n",
            "a3 OK LIST completed\r\n",
            "* MYRIGHTS \"Shared Folders/bob@test.local/INBOX\" rl\r\na4 OK done\r\n",
        ],
    )
    .await;
    let folders = folders(&provider(connection, Some(BOB))).await;
    assert_eq!(folders.len(), 1);
    let inbox = &folders[0];
    // Re-rooted: the store's INBOX is the Inbox, at the top of its tree, named as itself.
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));
    assert_eq!(inbox.parent, None);
    assert_eq!(inbox.name, "INBOX");
    assert_eq!(inbox.access, MailboxAccess::reader());
    assert_eq!(inbox.unread_count, Some(13));
    assert!(written(&recorded).contains(&format!(
        "a3 LIST \"\" \"{BOB}*\" RETURN (SPECIAL-USE STATUS (UNSEEN))"
    )));
}

#[tokio::test]
async fn a_shared_store_the_server_lists_nothing_of_is_an_error_not_an_empty_mailbox() {
    let (connection, _) = session(
        FULL,
        &[
            NAMESPACE,
            "a2 OK NAMESPACE completed\r\n",
            "a3 OK LIST completed\r\n",
        ],
    )
    .await;
    let err = provider(connection, Some(BOB))
        .sync_mailboxes(&account(), None)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.detail().contains("lost access"), "{err}");
}

#[tokio::test]
async fn a_refused_namespace_reads_as_none_and_the_own_folders_still_list() {
    // A `NO` is an answer with no namespaces in it, like a server without the extension —
    // not a failure that takes the folder list (and every Sent copy) down with it.
    let (connection, _) = session(
        &["NAMESPACE"],
        &[
            "a2 NO NAMESPACE not available\r\n",
            "* LIST () \"/\" \"INBOX\"\r\na3 OK LIST completed\r\n",
            "* STATUS \"INBOX\" (UNSEEN 1)\r\na4 OK done\r\n",
        ],
    )
    .await;
    let folders = folders(&provider(connection, None)).await;
    let ids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["INBOX"]);
}

#[tokio::test]
async fn a_refusal_is_not_kept_so_the_next_sync_asks_again() {
    // A `NO` can be passing (`[UNAVAILABLE]`). Kept for the provider's life, one would mix
    // every shared folder into the credential's own list until the host rebuilt it.
    let (connection, recorded) = session(
        &["NAMESPACE"],
        &[
            "a2 NO [UNAVAILABLE] try again later\r\n",
            "* LIST () \"/\" \"INBOX\"\r\na3 OK LIST completed\r\n",
            "* STATUS \"INBOX\" (UNSEEN 1)\r\na4 OK done\r\n",
            NAMESPACE,
            "a5 OK NAMESPACE completed\r\n",
            "* LIST () \"/\" \"INBOX\"\r\n",
            "* LIST () \"/\" \"Shared Folders/bob@test.local/INBOX\"\r\n",
            "a6 OK LIST completed\r\n",
            "* STATUS \"INBOX\" (UNSEEN 1)\r\na7 OK done\r\n",
        ],
    )
    .await;
    let provider = provider(connection, None);
    folders(&provider).await;
    let ids: Vec<String> = folders(&provider)
        .await
        .iter()
        .map(|f| f.id.as_str().to_owned())
        .collect();
    assert_eq!(written(&recorded).matches("NAMESPACE").count(), 2);
    // The second answer was read, so bob's store is no longer taken for alice's own.
    assert_eq!(ids, ["INBOX"]);
}

#[tokio::test]
async fn without_acl_nobody_is_asked_for_rights_and_every_folder_is_the_owners() {
    let (connection, recorded) = session(
        &["NAMESPACE"],
        &[
            "* NAMESPACE ((\"\" \"/\")) NIL NIL\r\na2 OK done\r\n",
            "* LIST () \"/\" \"INBOX\"\r\na3 OK LIST completed\r\n",
            // No LIST-STATUS either, so the one folder is probed for its count.
            "* STATUS \"INBOX\" (UNSEEN 1)\r\na4 OK done\r\n",
        ],
    )
    .await;
    let folders = folders(&provider(connection, None)).await;
    assert_eq!(folders[0].access, MailboxAccess::owner());
    assert_eq!(folders[0].unread_count, Some(1));
    assert!(!written(&recorded).contains("MYRIGHTS"));
}

#[tokio::test]
async fn list_myrights_reads_the_rights_off_the_folder_list_itself() {
    // Dovecot's shape on rev1: each row's `LIST`, then its `STATUS`, then its `MYRIGHTS`,
    // names in modified UTF-7 on all three.
    let (connection, recorded) = session(
        &[
            "NAMESPACE",
            "ACL",
            "LIST-STATUS",
            "SPECIAL-USE",
            "LIST-MYRIGHTS",
        ],
        &[
            "* NAMESPACE ((\"\" \"/\")) ((\"shared/\" \"/\")) NIL\r\na2 OK done\r\n",
            "* LIST () \"/\" INBOX\r\n* STATUS INBOX (UNSEEN 2)\r\n",
            "* MYRIGHTS INBOX lrwstipekxacd\r\n",
            "* LIST () \"/\" &ANw-berweisungen\r\n* STATUS &ANw-berweisungen (UNSEEN 0)\r\n",
            "* MYRIGHTS &ANw-berweisungen lr\r\n",
            // A row whose rights the server could not look up has no `MYRIGHTS` (RFC 8440
            // §3) — unknown, which must not read as "no rights".
            "* LIST (\\Sent) \"/\" Sent\r\n* STATUS Sent (UNSEEN 0)\r\n",
            "a3 OK List completed\r\n",
        ],
    )
    .await;
    let folders = folders(&provider(connection, None)).await;
    let access = |id: &str| folders.iter().find(|f| f.id.as_str() == id).unwrap().access;
    assert_eq!(access("INBOX"), MailboxAccess::owner());
    assert_eq!(access("Überweisungen"), MailboxAccess::reader());
    assert_eq!(access("Sent"), MailboxAccess::owner());
    let sent = written(&recorded);
    assert!(
        sent.ends_with("a3 LIST \"\" \"*\" RETURN (SPECIAL-USE STATUS (UNSEEN) MYRIGHTS)\r\n"),
        "one command answered everything, and nothing was pipelined after it: {sent}"
    );
}

#[tokio::test]
async fn the_probing_fallback_counts_only_the_stores_own_folders() {
    // A server without LIST-STATUS pays a STATUS per folder; paying for another
    // principal's folders too would be cost for rows that are then thrown away.
    let (connection, recorded) = session(
        &["NAMESPACE"],
        &[
            NAMESPACE,
            "a2 OK done\r\n",
            "* LIST () \"/\" \"INBOX\"\r\n",
            "* LIST () \"/\" \"Shared Folders/bob@test.local/INBOX\"\r\n",
            "a3 OK LIST completed\r\n",
            "* STATUS \"INBOX\" (UNSEEN 2)\r\na4 OK done\r\n",
        ],
    )
    .await;
    let folders = folders(&provider(connection, None)).await;
    assert_eq!(folders.len(), 1);
    let sent = written(&recorded);
    assert!(sent.contains("STATUS \"INBOX\""));
    assert!(!sent.contains("STATUS \"Shared Folders"));
}

#[tokio::test]
async fn discovery_lists_one_store_per_owner_and_caches_the_namespaces() {
    let (connection, recorded) = session(
        FULL,
        &[
            NAMESPACE,
            "a2 OK done\r\n",
            "* LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n",
            "* LIST (\\NoSelect) \"/\" \"Shared Folders/bob@test.local\"\r\n",
            "a3 OK LIST completed\r\n",
            // A second call asks LIST again but not NAMESPACE.
            "* LIST (\\NoSelect) \"/\" \"Shared Folders/support@test.local\"\r\n",
            "a4 OK LIST completed\r\n",
        ],
    )
    .await;
    let provider = provider(connection, None);
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );
    let stores = provider.list_shared_mailboxes().await.unwrap();
    let handles: Vec<&str> = stores.iter().map(|s| s.handle.as_str()).collect();
    assert_eq!(handles, [SUPPORT, BOB]);
    assert_eq!(stores[1].address.as_deref(), Some("bob@test.local"));
    // The address resolves through the list, to the handle discovery reported.
    let resolved = provider
        .resolve_shared_mailbox("Support@Test.Local")
        .await
        .unwrap();
    assert_eq!(resolved.handle.as_str(), SUPPORT);
    let sent = written(&recorded);
    assert!(sent.contains("a3 LIST \"\" \"Shared Folders/%\""));
    assert_eq!(sent.matches("NAMESPACE").count(), 1);
}

#[tokio::test]
async fn a_server_that_cannot_share_offers_no_discovery() {
    // Gmail and Outlook.com advertise NAMESPACE but no ACL: nothing is ever shared there
    // over IMAP, so a picker would never fill.
    let (connection, recorded) = session(&["NAMESPACE"], &[]).await;
    let provider = provider(connection, None);
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    assert_eq!(
        provider.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    assert!(!written(&recorded).contains("NAMESPACE"));
}

/// The first item a provider bound to the group's store yields for `folder`, and what it sent.
async fn first_streamed(folder: &str, server: &[&str]) -> (engine_provider::ProviderError, String) {
    let mut script = vec![NAMESPACE, "a2 OK NAMESPACE completed\r\n"];
    script.extend_from_slice(server);
    let (connection, recorded) = session(FULL, &script).await;
    let mut provider =
        ImapProvider::with_connection(connection, MailboxId::try_from(folder).unwrap());
    provider.shared_mailbox = Some(SharedMailboxId::try_from(SUPPORT).unwrap());
    let account = account();
    let err = {
        let window = engine_core::sync::SyncWindow::full();
        let mut stream = provider.stream_email(&account, None, window, 0, 0);
        stream.next().await.unwrap().unwrap_err()
    };
    (err, written(&recorded))
}

#[tokio::test]
async fn a_shared_store_never_syncs_a_folder_outside_it() {
    // Alice's own INBOX would put her mail in the shared mailbox's account; a store whose
    // name merely begins with the group's is somebody else's altogether.
    for outside in ["INBOX", "Shared Folders/support@test.local.invalid/INBOX"] {
        let (err, sent) = first_streamed(outside, &[]).await;
        assert_eq!(err.class(), FailureClass::InvalidState, "{outside}");
        assert!(
            !sent.contains("SELECT") && !sent.contains("EXAMINE"),
            "{sent}"
        );
    }
}

#[tokio::test]
async fn a_shared_store_syncs_its_own_folders() {
    // Past the check, the stream selects the folder; the scripted refusal proves it got there.
    let (_, sent) = first_streamed(
        "Shared Folders/support@test.local/INBOX",
        &["a3 NO [NONEXISTENT] no such mailbox\r\n"],
    )
    .await;
    assert!(
        sent.contains("\"Shared Folders/support@test.local/INBOX\""),
        "{sent}"
    );
}

#[tokio::test]
async fn placement_takes_the_stores_own_sent_whatever_order_the_server_lists() {
    // The group's `\Sent` comes first here — Stalwart lists it second, which is why a
    // first-match lookup looked right there and would not elsewhere.
    let namespaces = parse_namespace(
        &[b"NAMESPACE ((\"\" \"/\")) ((\"Shared Folders\" \"/\")) NIL".to_vec()],
        false,
    );
    let listing = [
        "* LIST (\\Sent) \"/\" \"Shared Folders/support@test.local/Sent Items\"\r\n",
        "* LIST (\\Sent) \"/\" \"Sent Items\"\r\n",
        "a2 OK LIST completed\r\n",
    ];
    let (mut connection, _) = session(&["SPECIAL-USE"], &listing).await;
    let own = MailStore::own(&namespaces);
    let folder = resolve_filing_folder(&mut connection, &own, Filing::Sent)
        .await
        .unwrap();
    assert_eq!(folder, "Sent Items");

    let (mut connection, _) = session(&["SPECIAL-USE"], &listing).await;
    let group = MailStore::shared(&namespaces, SUPPORT).unwrap();
    let folder = resolve_filing_folder(&mut connection, &group, Filing::Sent)
        .await
        .unwrap();
    assert_eq!(folder, format!("{SUPPORT}/Sent Items"));
}

#[tokio::test]
async fn a_store_without_the_role_gets_its_folder_created_inside_itself() {
    // Bob's share exposes only his INBOX. An unqualified fallback would create — and file
    // into — `Drafts` among alice's own folders.
    let namespaces = parse_namespace(
        &[b"NAMESPACE ((\"\" \"/\")) ((\"Shared Folders\" \"/\")) NIL".to_vec()],
        false,
    );
    let (mut connection, recorded) = session(
        &["SPECIAL-USE"],
        &[
            "* LIST () \"/\" \"Shared Folders/bob@test.local/INBOX\"\r\na2 OK LIST completed\r\n",
            "a3 NO [CANNOT] You are not allowed to create root folders under shared folders.\r\n",
        ],
    )
    .await;
    let store = MailStore::shared(&namespaces, BOB).unwrap();
    let folder = resolve_filing_folder(&mut connection, &store, Filing::Drafts)
        .await
        .unwrap();
    assert_eq!(folder, format!("{BOB}/Drafts"));
    assert!(written(&recorded).contains(&format!("a3 CREATE \"{BOB}/Drafts\"")));
}
