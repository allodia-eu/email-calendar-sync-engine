//! The [`Provider`] implementation: a Microsoft Graph client bound to one mail
//! folder for email, with the folder list synced at the account level.
//!
//! Graph mail `delta` is per-folder (`jmap.md`/this crate's docs), so — like
//! `provider-imap` — a [`GraphProvider`] is bound to a single folder: its
//! [`email_scope`](Provider::email_scope) names that folder
//! ([`SyncScope::GraphFolder`]) and [`stream_email`](Provider::stream_email)
//! streams its `messages/delta`. The folder list syncs under the per-account
//! [`SyncScope::GraphFolderList`]. The cross-folder fan-out is the orchestrator's
//! job.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    Capabilities, ConnectionInfo, EmailChunk, EmailStream, PageToken, PassMode, Provider,
    ProviderResult, ScopeSync, SyncKind, split_page,
};

use crate::{fetch, transport::GraphClient};

/// The folder list is re-discovered as a snapshot each pass (`GET /me/mailFolders`),
/// so it carries no provider cursor of its own — like IMAP's folder list.
const FOLDER_LIST_CURSOR: &str = "graph-folders";

/// A Microsoft Graph read/sync provider bound to one mail folder for email.
///
/// Construct one with [`GraphProvider::new`] from a connected
/// [`GraphClient`](crate::GraphClient) and the folder to bind. It advertises mail
/// read/sync; submission and calendar are later slices.
pub struct GraphProvider {
    client: GraphClient,
    folder: MailboxId,
    capabilities: Capabilities,
    /// The sync-depth cutoff the whole-scope drain ([`Provider::sync_email`]) syncs
    /// under via [`Provider::default_sync_window`]: when set, its initial message
    /// snapshot is windowed to messages received on or after this date (`None` syncs
    /// the whole folder). The streaming [`Provider::stream_email`] takes its window
    /// per call instead.
    since: Option<CalendarDate>,
}

impl core::fmt::Debug for GraphProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphProvider")
            .field("folder", &self.folder)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl GraphProvider {
    /// Binds a connected client to one mail folder for email sync.
    #[must_use]
    pub fn new(client: GraphClient, folder: MailboxId) -> Self {
        Self {
            client,
            folder,
            capabilities: Capabilities::none().with_mail(),
            since: None,
        }
    }

    /// Sets the default sync-depth cutoff for the whole-scope [`Provider::sync_email`]
    /// drain: its initial message snapshot is windowed to messages received on or after
    /// `since`. Later incremental syncs follow the server's deltaLink, which carries the
    /// window. Streaming callers pass a window per call to [`Provider::stream_email`].
    #[must_use]
    pub fn with_since(mut self, since: CalendarDate) -> Self {
        self.since = Some(since);
        self
    }
}

#[async_trait]
impl Provider for GraphProvider {
    /// The fixed mail capabilities plus the transport's negotiated HTTP version.
    ///
    /// Graph has no session-discovery step, so [`GraphClient::connect`] issues no
    /// request and the HTTP version is `None` until this provider's first fetch —
    /// unlike JMAP/CalDAV, which learn it while connecting. The TLS version is always
    /// `None`: reqwest exposes only the peer certificate, never the negotiated
    /// protocol version (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphFolderList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphFolder {
            account: account.clone(),
            folder: self.folder.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let mailboxes = fetch::folders(&self.client).await?;
        // `GET /me/mailFolders` is a full snapshot every pass, so every folder is present.
        let present: BTreeSet<ProviderKey> = mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mailboxes, present),
            SyncState::new(FOLDER_LIST_CURSOR),
        ))
    }

    /// The whole-scope [`Provider::sync_email`] drain windows under the cutoff fixed at
    /// construction ([`GraphProvider::with_since`]); [`Provider::stream_email`] takes its
    /// window per call.
    fn default_sync_window(&self) -> SyncWindow {
        self.since.map_or_else(SyncWindow::full, SyncWindow::since)
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        // Graph consumer `messages/delta` page size is server-controlled (`graph.md`),
        // so the fetch-batch knob has no lever here; each server page is drained whole.
        _fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        // A sync-depth window bounds a snapshot via a `receivedDateTime` `$filter`; a
        // delta ignores it (new arrivals are recent, and the deltaLink carries the
        // window).
        let floor = window.floor();
        Box::pin(async_stream::try_stream! {
            // Each Graph page is fetched whole over HTTP and re-chunked for incremental
            // commit; intermediate chunks hold the cursor and a final marker advances it
            // (a delta is not cheaply resumable mid-pass).
            let mut page_token: Option<PageToken> = None;
            let mut mode: Option<PassMode> = None;
            let mut total: Option<usize> = None;
            let final_cursor = loop {
                let page = fetch::messages_page(
                    &self.client,
                    &self.folder,
                    cursor,
                    page_token.as_ref(),
                    floor,
                )
                .await?;
                total = total.or(page.total);
                // Decide the pass mode once, from the first page: a snapshot (first sync)
                // reconciles — its present set tombstones absent rows; a delta is additive.
                let pass_mode = *mode.get_or_insert(match page.kind {
                    SyncKind::Snapshot => PassMode::Reconcile,
                    SyncKind::Delta => PassMode::Additive,
                });
                let is_last = page.next_page.is_none();
                let next_cursor = page.next_cursor.clone();
                for chunk in split_page(
                    pass_mode,
                    page.changed,
                    page.removed,
                    page.present,
                    total,
                    chunk_size,
                ) {
                    yield chunk;
                }
                if is_last {
                    break next_cursor;
                }
                page_token = page.next_page;
            };
            // The final marker carries the cursor (and, for reconcile, tombstones against
            // the accumulated present set).
            yield match mode.unwrap_or(PassMode::Additive) {
                PassMode::Additive => {
                    EmailChunk::additive(Vec::new(), Vec::new(), total, final_cursor)
                }
                PassMode::Reconcile => {
                    EmailChunk::reconcile_last(Vec::new(), Vec::new(), total, final_cursor)
                }
            };
        })
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        // Graph streams a message's full RFC 822 MIME from `/messages/{id}/$value`;
        // the message's provider key is that immutable id. One credential (the bound
        // client's token) backs the fetch, like every other call on this provider.
        Ok(fetch::message_source(&self.client, message.id.key()).await?)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use serde_json::json as jval;

    use super::*;
    use crate::test_support::{fake_client, folder_routes, json};

    const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");
    const CHANGED_FULL: &str =
        include_str!("../tests/fixtures/mail/messages_delta_changed_full.json");

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
        // A fixture-fed fake transport speaks no HTTP and reqwest never reports TLS.
        assert_eq!(info.http_version, None);
        assert_eq!(info.tls_version, None);
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
        let client =
            GraphClient::with_base("fake-token", base, crate::test_support::tls()).unwrap();
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
        )
        .unwrap();
        assert!(client.get(&client.url("/me/nope")).await.is_err());
    }
}
