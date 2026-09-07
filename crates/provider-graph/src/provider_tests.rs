//! Unit tests for the Graph [`Provider`] adapter (`super`) — the folder-list scope,
//! the `messages/delta` pass (snapshot vs delta, paging, chunking), and the recovery
//! when a stored deltaLink has aged out. Split out to keep `provider.rs` under the
//! 500-line limit (AGENTS.md).

use futures_util::StreamExt;
use serde_json::json as jval;

use super::*;
use crate::test_support::{fake_client, fake_client_fallible, folder_routes, json};

const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");
const CHANGED_FULL: &str = include_str!("../tests/fixtures/mail/messages_delta_changed_full.json");

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// Drains an email chunk stream into its chunks, so a test asserts the pass's
/// aggregate shape (the intra-pass paging is the adapter's internal detail).
async fn drain(mut stream: EmailStream<'_>) -> Vec<EmailChunk> {
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }
    chunks
}

/// The number of messages upserted across a drained pass's chunks.
fn upserted(chunks: &[EmailChunk]) -> usize {
    chunks.iter().map(|c| c.changed.len()).sum()
}

/// A synthetic `messages/delta` page: full messages (each carries `@odata.etag`, so
/// no re-fetch) plus the continuation link that decides whether the pass ends.
fn delta_page(ids: &[&str], link_key: &str, link: &str) -> serde_json::Value {
    let value: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| jval!({ "id": id, "parentFolderId": "folder-inbox", "@odata.etag": "e" }))
        .collect();
    jval!({ "value": value, link_key: link })
}

#[tokio::test]
async fn advertises_per_folder_scopes_and_mail_capability() {
    let folder = MailboxId::try_from("folder-inbox").unwrap();
    let provider = GraphProvider::new(fake_client(vec![]), folder.clone());
    let info = provider.connection_info();
    assert!(info.capabilities.mail());
    // Mutating writes and submission are advertised alongside read/sync.
    assert!(info.capabilities.mail_writes());
    assert!(info.capabilities.submission());
    // A fixture-fed fake transport speaks no HTTP and reqwest never reports TLS.
    assert_eq!(info.http_version, None);
    assert_eq!(info.tls_version, None);
    // And the mailbox's concurrency ceiling, so a caller draining single fetches (the body
    // warm) overlaps them instead of paying a round trip each.
    assert_eq!(
        info.concurrent_fetches,
        crate::provider::MAX_CONCURRENT_SOURCE_FETCHES
    );
    assert_eq!(
        provider.mailbox_scope(&account()),
        SyncScope::GraphFolderList { account: account() }
    );
    assert_eq!(
        provider.email_scope(&account()),
        SyncScope::GraphFolder {
            account: account(),
            folder,
        }
    );
}

#[tokio::test]
async fn edit_mail_flows_through_the_provider_to_the_write() {
    // The `Provider::edit_mail` wrapper delegates to `crate::mutate`; a delete routed to a
    // 204 (no body) resolves with the target key.
    let target = engine_core::ids::ProviderKey::new("message-write").unwrap();
    let client = fake_client(vec![("/permanentDelete", serde_json::Value::Null)]);
    let provider = GraphProvider::new(client, MailboxId::try_from("folder-inbox").unwrap());
    let receipt = provider
        .edit_mail(&account(), &MailEdit::delete(target.clone()))
        .await
        .unwrap();
    assert_eq!(receipt.message_key, target);
    assert!(format!("{provider:?}").contains("GraphProvider"));
}

#[tokio::test]
async fn syncs_the_folder_list_and_a_message_snapshot_page() {
    let folder = MailboxId::try_from("folder-inbox").unwrap();
    let mut routes = folder_routes();
    routes.push(("messages/delta", json(SNAPSHOT)));
    let provider = GraphProvider::new(fake_client(routes), folder);

    let folders = provider.sync_mailboxes(&account(), None).await.unwrap();
    assert!(folders.is_snapshot());

    // The whole-scope drain streams `stream_email`; the single snapshot page holds 3.
    let email = provider.sync_email(&account(), None).await.unwrap();
    assert!(email.is_snapshot());
}

#[tokio::test]
async fn snapshot_stream_reconciles_and_chunks_a_single_page() {
    // The initial pass (no cursor) is a snapshot → a reconciling pass whose present
    // set drives end-of-pass tombstoning. `chunk_size = 2` splits the 3-message page
    // into two content chunks, then a final marker tombstones and advances.
    let provider = GraphProvider::new(
        fake_client(vec![("messages/delta", json(SNAPSHOT))]),
        MailboxId::try_from("folder-inbox").unwrap(),
    );

    let chunks = drain(provider.stream_email(&account(), None, SyncWindow::full(), 0, 2)).await;
    assert_eq!(upserted(&chunks), 3);
    let present: usize = chunks.iter().map(|c| c.present.len()).sum();
    assert_eq!(present, 3, "every snapshot id rides the reconcile chunks");
    // Graph consumer delta advertises no total, and only the final chunk advances.
    assert_eq!(chunks[0].total, None);
    assert_eq!(
        chunks.iter().filter(|c| c.advance_to.is_some()).count(),
        1,
        "intermediate chunks hold the cursor"
    );
    let last = chunks.last().unwrap();
    assert!(last.is_reconcile_final());
    assert!(
        last.advance_to
            .as_ref()
            .unwrap()
            .as_str()
            .contains("deltatoken")
    );
}

#[tokio::test]
async fn snapshot_stream_drains_multiple_pages() {
    // Page one reports an `@odata.nextLink`; page two is terminal (`@odata.deltaLink`).
    // The stream drains both before the final marker (skiptoken route first, so it
    // wins over the initial `messages/delta` substring match).
    let page1 = delta_page(
        &["m1"],
        "@odata.nextLink",
        "https://graph.microsoft.com/v1.0/me/mailFolders/folder-inbox/messages/delta?$skiptoken=PAGE2",
    );
    let page2 = delta_page(
        &["m2"],
        "@odata.deltaLink",
        "https://graph.microsoft.com/v1.0/me/mailFolders/folder-inbox/messages/delta?$deltatoken=FINAL",
    );
    let provider = GraphProvider::new(
        fake_client(vec![("skiptoken=PAGE2", page2), ("messages/delta", page1)]),
        MailboxId::try_from("folder-inbox").unwrap(),
    );

    let chunks = drain(provider.stream_email(&account(), None, SyncWindow::full(), 0, 0)).await;
    assert_eq!(upserted(&chunks), 2, "both pages drained");
    let present: usize = chunks.iter().map(|c| c.present.len()).sum();
    assert_eq!(present, 2);
    let last = chunks.last().unwrap();
    assert!(last.is_reconcile_final());
    assert!(
        last.advance_to
            .as_ref()
            .unwrap()
            .as_str()
            .contains("deltatoken=FINAL")
    );
}

#[tokio::test]
async fn delta_stream_is_additive_and_advances_to_the_delta_link() {
    // A pass resumed from a cursor is a delta → additive (never tombstones). A full
    // changed entry (with `@odata.etag`) is used directly, and the terminal
    // `@odata.deltaLink` becomes the advanced cursor.
    let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
    let provider = GraphProvider::new(
        fake_client(vec![("delta-token-1", json(CHANGED_FULL))]),
        MailboxId::try_from("folder-inbox").unwrap(),
    );

    let chunks =
        drain(provider.stream_email(&account(), Some(&cursor), SyncWindow::full(), 0, 0)).await;
    assert_eq!(upserted(&chunks), 1);
    assert_eq!(chunks.iter().map(|c| c.present.len()).sum::<usize>(), 0);
    let last = chunks.last().unwrap();
    assert_eq!(last.mode, PassMode::Additive);
    assert!(!last.is_reconcile_final(), "a delta never tombstones");
    assert!(
        last.advance_to
            .as_ref()
            .unwrap()
            .as_str()
            .contains("deltatoken")
    );
}

/// The body Graph actually returns once a stored deltaLink has aged out.
fn sync_state_not_found() -> serde_json::Value {
    jval!({
        "error": {
            "code": "SyncStateNotFound",
            "message": "The sync state generation is not found; generation=108;[highest=143]."
        }
    })
}

#[tokio::test]
async fn an_aged_out_delta_link_restarts_the_pass_as_a_full_snapshot() {
    // Graph expires a stored deltaLink and answers `410 SyncStateNotFound`. That cursor can
    // never produce a delta again, so the pass has to drop it and re-enumerate the folder.
    // Without that the folder is wedged for good: every pass replays the same dead cursor,
    // upserts nothing, and no new mail is ever delivered again.
    let cursor =
        SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta?$deltatoken=STALE");
    let provider = GraphProvider::new(
        fake_client_fallible(vec![
            ("deltatoken=STALE", Err((410, sync_state_not_found()))),
            ("messages/delta", Ok(json(SNAPSHOT))),
        ]),
        MailboxId::try_from("folder-inbox").unwrap(),
    );

    let chunks =
        drain(provider.stream_email(&account(), Some(&cursor), SyncWindow::full(), 0, 0)).await;

    assert_eq!(
        upserted(&chunks),
        3,
        "the folder re-enumerated from scratch"
    );
    let last = chunks.last().unwrap();
    assert_eq!(
        last.mode,
        PassMode::Reconcile,
        "the recovered pass is a snapshot, so it reconciles instead of appending"
    );
    assert_eq!(
        chunks.iter().map(|c| c.present.len()).sum::<usize>(),
        3,
        "every re-enumerated id rides the present set the tombstoning is computed against"
    );
    assert!(
        !last.advance_to.as_ref().unwrap().as_str().contains("STALE"),
        "the dead cursor is replaced, never re-persisted"
    );
}

#[tokio::test]
async fn the_resync_restarts_the_whole_pass_so_every_page_feeds_the_present_set() {
    // Guards the *shape* of the recovery, not just that it recovers. Recovering only the one
    // failing call — refetching it without the cursor while later pages still carried it —
    // would leave those later pages classed `Delta`, contributing nothing to `present`, while
    // the pass as a whole reconciles from page one. End-of-pass tombstoning would then delete
    // every message those pages returned: a sync stall turned into data loss. So after the
    // fallback, *both* pages must feed `present`.
    let page1 = delta_page(
        &["m1"],
        "@odata.nextLink",
        "https://graph.test/me/mailFolders/folder-inbox/messages/delta?$skiptoken=PAGE2",
    );
    let page2 = delta_page(
        &["m2"],
        "@odata.deltaLink",
        "https://graph.test/me/mailFolders/folder-inbox/messages/delta?$deltatoken=FRESH",
    );
    let cursor = SyncState::new(
        "https://graph.test/me/mailFolders/folder-inbox/messages/delta?$deltatoken=STALE",
    );
    let provider = GraphProvider::new(
        fake_client_fallible(vec![
            ("deltatoken=STALE", Err((410, sync_state_not_found()))),
            ("skiptoken=PAGE2", Ok(page2)),
            ("messages/delta", Ok(page1)),
        ]),
        MailboxId::try_from("folder-inbox").unwrap(),
    );

    let chunks =
        drain(provider.stream_email(&account(), Some(&cursor), SyncWindow::full(), 0, 0)).await;

    assert_eq!(
        upserted(&chunks),
        2,
        "both pages of the restarted pass drained"
    );
    assert_eq!(
        chunks.iter().map(|c| c.present.len()).sum::<usize>(),
        2,
        "page two feeds `present` too — otherwise the reconcile tombstones it"
    );
    assert!(chunks.last().unwrap().is_reconcile_final());
}

/// Drains the snapshot to a real [`Message`] whose provider key drives the
/// `$value` fetch, mirroring the reading path.
async fn first_snapshot_message(provider: &GraphProvider) -> Message {
    let email = provider.sync_email(&account(), None).await.unwrap();
    match email.update {
        SyncUpdate::Snapshot { objects, .. } => objects.into_iter().next().unwrap(),
        SyncUpdate::Delta { changed, .. } => changed.into_iter().next().unwrap(),
    }
}

#[tokio::test]
async fn fetch_message_source_returns_the_raw_mime_from_the_value_endpoint() {
    // `$value` streams the full RFC 822 MIME, carried here as a raw (string) route.
    const MIME: &str = "From: a@example.com\r\nSubject: Hi\r\n\r\nBody text\r\n";
    let folder = MailboxId::try_from("folder-inbox").unwrap();
    let routes = vec![
        ("messages/delta", json(SNAPSHOT)),
        ("/$value", serde_json::Value::String(MIME.to_owned())),
    ];
    let provider = GraphProvider::new(fake_client(routes), folder);

    let message = first_snapshot_message(&provider).await;
    let raw = provider
        .fetch_message_source(&account(), &message)
        .await
        .unwrap();
    assert_eq!(raw.as_bytes(), MIME.as_bytes());
}

#[tokio::test]
async fn fetch_message_source_errors_when_the_source_is_unavailable() {
    // No `$value` route (the message is gone / not routed) → a classified error,
    // not a panic — so the reading view surfaces "couldn't load" rather than crash.
    let folder = MailboxId::try_from("folder-inbox").unwrap();
    let provider = GraphProvider::new(
        fake_client(vec![("messages/delta", json(SNAPSHOT))]),
        folder,
    );
    let message = first_snapshot_message(&provider).await;
    assert!(
        provider
            .fetch_message_source(&account(), &message)
            .await
            .is_err()
    );
}

/// The full folder + message sync, in path-priority order (most specific first):
/// a delta resume, a changed-id re-fetch, the snapshot, then the folder routes.
fn replay_routes() -> Vec<(&'static str, serde_json::Value)> {
    let mut routes = vec![
        (
            "$deltatoken=",
            json(include_str!(
                "../tests/fixtures/mail/messages_delta_changed.json"
            )),
        ),
        (
            "/me/messages/",
            json(include_str!("../tests/fixtures/mail/message_detail.json")),
        ),
        ("messages/delta", json(SNAPSHOT)),
    ];
    routes.extend(folder_routes());
    routes
}

#[tokio::test]
async fn end_to_end_against_a_fixture_replay_server() {
    // Drive the whole stack — reqwest transport + URL rebasing + fetch
    // orchestration — over real HTTP against the captured fixtures, no token.
    // Role/field assertions live in the in-process fake tests; this proves the
    // real-HTTP path end to end (every call succeeding is the assertion).
    let base = crate::test_support::replay_server(replay_routes());
    let client = GraphClient::with_base(
        "fake-token",
        base,
        crate::test_support::tls(),
        crate::test_support::retry(),
    )
    .unwrap();
    let provider = GraphProvider::new(client, MailboxId::try_from("folder-inbox").unwrap());

    // Folder list (7 well-known GETs + the list) over HTTP.
    assert!(
        provider
            .sync_mailboxes(&account(), None)
            .await
            .unwrap()
            .is_snapshot()
    );
    // The message snapshot, then a delta resumed from its cursor whose partial
    // change is re-fetched (a failed re-fetch would error the call) — following
    // the rebased absolute deltaLink + re-fetch URLs end to end.
    let snapshot = provider.sync_email(&account(), None).await.unwrap();
    assert!(snapshot.is_snapshot());
    let delta = provider
        .sync_email(&account(), Some(&snapshot.next_cursor))
        .await
        .unwrap();
    assert!(!delta.is_snapshot());
}

#[tokio::test]
async fn replay_server_404s_an_unrouted_path() {
    // An unrouted request → the server's 404 → a classified Status error.
    let client = GraphClient::with_base(
        "t",
        crate::test_support::replay_server(vec![]),
        crate::test_support::tls(),
        crate::test_support::retry(),
    )
    .unwrap();
    assert!(client.get(&client.url("/me/nope")).await.is_err());
}

/// The one URL this read may send. The fake routes by substring and errors on a miss,
/// so keying the route on the whole path **is** the request assertion: a `$select` that
/// dropped `displayName` would return a user resource with no name in it and read as
/// "the server holds none", which is indistinguishable from a correct empty answer.
const IDENTITY_URL: &str = "/me?$select=displayName,mail,userPrincipalName";

const ME_IDENTITY: &str = include_str!("../tests/fixtures/mail/me_identity.json");

#[tokio::test]
async fn the_captured_principal_yields_one_identity_out_of_a_full_default_payload() {
    // The bytes a real personal account returned. `$select` is acknowledged in
    // `@odata.context` and **not applied** (measured, `graph.md`), so what actually arrives is
    // every default `user` property: `ageGroup`, `businessPhones`, `givenName`, `preferredLanguage`
    // and the rest. The normalizer has to pick its three out of that and ignore the noise,
    // which the hand-written three-field cases above cannot show.
    let client = fake_client(vec![(IDENTITY_URL, json(ME_IDENTITY))]);
    let provider = GraphProvider::new(client, MailboxId::try_from("inbox").unwrap());

    let identities = provider.sender_identities(&account()).await.unwrap();

    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].address.email, "testuser@example.test");
    assert_eq!(identities[0].address.name.as_deref(), Some("Test User"));
}

#[tokio::test]
async fn the_sender_identity_is_read_from_the_mailbox_principal() {
    let client = fake_client(vec![(
        IDENTITY_URL,
        jval!({
            "displayName": "Alice Smith",
            "mail": "alice@example.com",
            "userPrincipalName": "alice@tenant.example.test"
        }),
    )]);
    let provider = GraphProvider::new(client, MailboxId::try_from("inbox").unwrap());

    let identities = provider.sender_identities(&account()).await.unwrap();

    assert_eq!(identities.len(), 1, "one principal, one identity");
    assert_eq!(identities[0].address.name.as_deref(), Some("Alice Smith"));
    assert_eq!(identities[0].address.email, "alice@example.com");
}

#[tokio::test]
async fn a_shared_mailbox_reads_its_own_identity_not_the_signed_in_user() {
    // Each mailbox is a separate engine account differing only by its principal
    // (`crate::MailboxPrincipal`), so the name must follow the principal — otherwise
    // every shared mailbox would be labelled with the delegate's own name.
    let client = fake_client(vec![(
        "/users/info@example.org?$select=displayName",
        jval!({ "displayName": "Info Desk", "mail": "info@example.org" }),
    )])
    .with_principal(crate::MailboxPrincipal::user("info@example.org"));
    let provider = GraphProvider::new(client, MailboxId::try_from("inbox").unwrap());

    let identities = provider.sender_identities(&account()).await.unwrap();

    assert_eq!(identities[0].address.name.as_deref(), Some("Info Desk"));
}

#[test]
fn graph_advertises_a_read_only_directory_name() {
    // A host reads this to decide whether to draw an editor. Claiming `Writable` here
    // would offer an edit only a tenant administrator can make.
    let provider = GraphProvider::new(fake_client(vec![]), MailboxId::try_from("inbox").unwrap());
    assert_eq!(
        provider.connection_info().capabilities.sender_identities(),
        Some(IdentityControls::ReadOnly)
    );
}
