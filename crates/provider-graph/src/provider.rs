//! The [`Provider`] implementation: a Microsoft Graph client bound to one mail
//! folder for email, with the folder list synced at the account level.
//!
//! Graph mail `delta` is per-folder (`jmap.md`/this crate's docs), so — like
//! `provider-imap` — a [`GraphProvider`] is bound to a single folder: its
//! [`email_scope`](Provider::email_scope) names that folder
//! ([`SyncScope::GraphFolder`]) and [`sync_email_page`](Provider::sync_email_page)
//! pages its `messages/delta`. The folder list syncs under the per-account
//! [`SyncScope::GraphFolderList`]. The cross-folder fan-out is the orchestrator's
//! job.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::ids::{AccountId, MailboxId, ProviderKey};
use engine_core::mail::{Mailbox, Message};
use engine_core::sync::{SyncScope, SyncState, SyncUpdate};
use engine_provider::{Capabilities, PageToken, Provider, ProviderResult, ScopeSync, SyncPage};

use crate::fetch;
use crate::transport::GraphClient;

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
        }
    }
}

#[async_trait]
impl Provider for GraphProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
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

    async fn sync_email_page(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        _limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        Ok(fetch::messages_page(&self.client, &self.folder, cursor, page).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_client, folder_routes, json};

    const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");

    fn account() -> AccountId {
        AccountId::try_from("acct-1").unwrap()
    }

    #[tokio::test]
    async fn advertises_per_folder_scopes_and_mail_capability() {
        let folder = MailboxId::try_from("folder-inbox").unwrap();
        let provider = GraphProvider::new(fake_client(vec![]), folder.clone());
        assert!(provider.capabilities().mail());
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

        // The drain default pages `sync_email_page`; the single snapshot page holds 3.
        let email = provider.sync_email(&account(), None).await.unwrap();
        assert!(email.is_snapshot());
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
        let client = GraphClient::with_base("fake-token", base).unwrap();
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
        let client =
            GraphClient::with_base("t", crate::test_support::replay_server(vec![])).unwrap();
        assert!(client.get(&client.url("/me/nope")).await.is_err());
    }
}
