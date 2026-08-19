//! One message key held by **two** of an account's mail scopes.
//!
//! This is a Microsoft Graph move mid-flight, not a corrupt store. Graph mail sync is per
//! folder — one `SyncScope::GraphFolder` and one `deltaLink` each — and a move keeps the
//! message's *immutable id* (live-verified; `provider-graph`'s `mutate` module and fixture
//! Finding 13). So the destination folder's delta creates the key before the source folder's
//! delta reports it `@removed`, and in between both scopes hold it.
//!
//! A host asking for the account's messages must get **one** message out of that, filed where
//! the message actually is — not the same mail twice, and not the folder it just left.

use engine_api::{Engine, StreamTuning};
use engine_provider::PassMode;
use engine_sync::IgnoreCommits;

use super::*;

/// A provider bound to one Graph-shaped folder, reporting one fixed message as an additive
/// stream — the shape an account pass drives per folder.
struct FolderProvider {
    folder: &'static str,
    message: Message,
}

impl FolderProvider {
    /// The same message key, filed in `folder` and last modified at `modified`.
    fn holding(folder: &'static str, modified: &str) -> Self {
        let mut message = message("msg-1", folder, "Quarterly report");
        message.received_at = Some("2026-06-01T09:00:00Z".parse().unwrap());
        message.last_modified = Some(modified.parse().unwrap());
        Self { folder, message }
    }
}

#[async_trait::async_trait]
impl Provider for FolderProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphFolderList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphFolder {
            account: account.clone(),
            folder: MailboxId::try_from(self.folder).unwrap(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let mailboxes = vec![
            mailbox("folder-inbox", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("folder-archive", "Archive", Some(MailboxRole::Archive)),
        ];
        let present = mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mailboxes, present),
            SyncState::new("folders-1"),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        _cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let chunk = EmailChunk {
            mode: PassMode::Additive,
            changed: vec![self.message.clone()],
            patched: Vec::new(),
            removed: Vec::new(),
            present: Vec::new(),
            total: Some(1),
            advance_to: Some(SyncState::new(format!("{}-cursor-1", self.folder))),
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

/// Syncs `source` (the folder the message is leaving, stale) then `destination` (where the move
/// put it, carrying the later `lastModifiedDateTime`), and asserts the account reads back as one
/// message filed in `destination`.
async fn move_in_flight_reads_as_one_message_in(source: &'static str, destination: &'static str) {
    let engine = Engine::open_in_memory().unwrap();
    let stale = FolderProvider::holding(source, "2026-06-01T09:00:00Z");
    let fresh = FolderProvider::holding(destination, "2026-06-02T10:00:00Z");

    // One pass per provider, deliberately: this case turns on the stale folder being seen
    // *before* the fresh one, and an account pass fans its folders out concurrently, which would
    // leave the order — and so the outcome — up to the scheduler.
    for provider in [&stale, &fresh] {
        engine
            .sync_mail(
                core::slice::from_ref(provider),
                &account(),
                StreamTuning::responsive(),
                &IgnoreCommits,
            )
            .await;
    }

    // Both scopes hold the key — the state this test exists for.
    let rows = engine
        .mail_by_keys(&account(), &[ProviderKey::new("msg-1").unwrap()])
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the store keys rows by (scope, key), so a move in flight is two rows"
    );

    let messages = engine.messages(&account()).await.unwrap();
    assert_eq!(
        messages.len(),
        1,
        "but it is one message, so a host's list must not show it twice"
    );
    assert_eq!(
        messages[0]
            .mailboxes
            .iter()
            .map(MailboxId::as_str)
            .collect::<Vec<_>>(),
        vec![destination],
        "and it is where the move put it — the fresher row wins on last_modified"
    );

    // The targeted resolve agrees with the list read; a host must not get one answer from the
    // window and a different one from a key lookup.
    let resolved = engine
        .messages_by_keys(&account(), &[ProviderKey::new("msg-1").unwrap()])
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0]
            .mailboxes
            .iter()
            .map(MailboxId::as_str)
            .collect::<Vec<_>>(),
        vec![destination]
    );
}

/// Archiving: out of the Inbox scope, into the Archive scope.
#[tokio::test]
async fn one_key_in_two_folder_scopes_reads_back_as_one_message_in_the_newer_folder() {
    move_in_flight_reads_as_one_message_in("folder-inbox", "folder-archive").await;
}

/// Un-archiving — the **same** scenario with the two folders swapped.
///
/// Both directions are asserted because the rows only differ in their `scope_key`, and a read
/// that resolved the duplicate by whichever scope it visited last would pass one direction and
/// fail the other. Running both is what makes this a test of `last_modified` rather than a test
/// of how `SyncScope` happens to serialize.
#[tokio::test]
async fn the_fresher_row_wins_whichever_way_the_two_scope_keys_sort() {
    move_in_flight_reads_as_one_message_in("folder-archive", "folder-inbox").await;
}
