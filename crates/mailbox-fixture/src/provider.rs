//! An offline [`Provider`] that serves one generated folder.
//!
//! The fixture reaches the store the way real mail does — claim, project, apply,
//! release — rather than by inserting rows behind the engine's back. That is what
//! makes a number measured against it a number about the code that ships: an apply
//! path shortcut would make the store look faster than any sync can ever be.
//!
//! IMAP-shaped on purpose. A JMAP account is one `Email` scope, so a fixture built
//! that way would hide the per-scope loop the windowed read pays for; an IMAP account
//! is one scope per folder, which is the shape that costs.

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Mailbox, Message},
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EmailChunk, EmailStream, Provider, ProviderResult, ScopeSync,
};

/// How many messages one streamed chunk carries during population.
///
/// Each chunk is one store transaction, so this trades transaction count against
/// transaction size. Two thousand keeps a 400k-message build to 200 commits without
/// holding an unreasonable batch in memory.
pub const POPULATE_CHUNK: usize = 2_000;

/// What a [`FolderProvider`] serves on its next email stream.
#[derive(Debug, Clone)]
pub enum Pass {
    /// A first full sync: every message, as reconciling pages that tombstone
    /// anything the fixture no longer lists.
    Snapshot(Vec<Message>),
    /// An incremental delta: these messages upserted, nothing tombstoned — the shape
    /// a flag change or a page of new mail arrives in.
    Delta(Vec<Message>),
}

/// A provider bound to one folder of a generated mailbox.
///
/// One instance per folder, exactly as a host binds one IMAP provider per mailbox.
#[derive(Debug, Clone)]
pub struct FolderProvider {
    mailbox: MailboxId,
    /// Every folder of the account: the mailbox-list scope is shared, so whichever
    /// provider syncs it must return the whole list.
    mailboxes: Vec<Mailbox>,
    pass: Pass,
}

impl FolderProvider {
    /// A provider for `mailbox` that serves `pass`, and whose mailbox-list sync
    /// returns `mailboxes`.
    #[must_use]
    pub fn new(mailbox: MailboxId, mailboxes: Vec<Mailbox>, pass: Pass) -> Self {
        Self {
            mailbox,
            mailboxes,
            pass,
        }
    }
}

#[async_trait::async_trait]
impl Provider for FolderProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailboxList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        // IMAP re-LISTs the folders every pass, so this is always a snapshot.
        let present = self
            .mailboxes
            .iter()
            .map(|mailbox| mailbox.id.key().clone())
            .collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
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
        Box::pin(futures_util::stream::iter(
            self.chunks().into_iter().map(Ok),
        ))
    }
}

impl FolderProvider {
    /// The chunks this pass yields.
    ///
    /// A snapshot is paged so the store commits in bounded transactions; a delta is a
    /// single additive chunk, which is what a flag change or a page of new mail is.
    fn chunks(&self) -> Vec<EmailChunk> {
        match &self.pass {
            Pass::Delta(messages) => vec![EmailChunk::additive(
                messages.clone(),
                Vec::new(),
                Some(messages.len()),
                SyncState::new("delta-1"),
            )],
            Pass::Snapshot(messages) => {
                let total = messages.len();
                if total == 0 {
                    return vec![EmailChunk::reconcile_last(
                        Vec::new(),
                        Vec::new(),
                        Some(0),
                        SyncState::new("snapshot-1"),
                    )];
                }
                let pages: Vec<&[Message]> = messages.chunks(POPULATE_CHUNK).collect();
                let last = pages.len() - 1;
                pages
                    .into_iter()
                    .enumerate()
                    .map(|(index, page)| {
                        let changed = page.to_vec();
                        let present: Vec<ProviderKey> =
                            page.iter().map(|m| m.id.key().clone()).collect();
                        if index == last {
                            EmailChunk::reconcile_last(
                                changed,
                                present,
                                Some(total),
                                SyncState::new("snapshot-1"),
                            )
                        } else {
                            EmailChunk::reconcile_page(changed, present, Some(total))
                        }
                    })
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{AccountId, MailboxId},
        mail::Mailbox,
    };
    use engine_provider::Provider as _;

    use super::{FolderProvider, POPULATE_CHUNK, Pass};
    use crate::{generate, spec::FixtureSpec};

    fn account() -> AccountId {
        AccountId::try_from("acct-1").expect("a valid account id")
    }

    fn provider(pass: Pass) -> FolderProvider {
        let id = MailboxId::try_from("INBOX").unwrap();
        FolderProvider::new(id.clone(), vec![Mailbox::new(id, "Inbox")], pass)
    }

    #[test]
    fn a_snapshot_pages_and_only_its_last_chunk_advances_the_cursor() {
        let fixture = generate(&FixtureSpec::new(account(), POPULATE_CHUNK * 2 + 5));
        let messages: Vec<_> = fixture.newest_first().into_iter().cloned().collect();
        let chunks = provider(Pass::Snapshot(messages.clone())).chunks();

        assert_eq!(chunks.len(), 3, "two full pages and a remainder");
        assert!(
            chunks[..2].iter().all(|chunk| chunk.advance_to.is_none()),
            "an intermediate page holds the cursor, so a kill mid-pass re-runs it"
        );
        assert!(chunks[2].is_reconcile_final(), "the last page tombstones");
        let covered: usize = chunks.iter().map(|chunk| chunk.present.len()).sum();
        assert_eq!(
            covered,
            messages.len(),
            "every key is claimed present exactly once, or the final chunk tombstones live mail"
        );
    }

    #[test]
    fn an_empty_snapshot_still_yields_a_final_chunk() {
        // The folder a fixture never filed anything into. Yielding nothing would leave
        // the scope cursorless, so the next pass would snapshot it again forever.
        let chunks = provider(Pass::Snapshot(Vec::new())).chunks();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_reconcile_final());
    }

    #[test]
    fn a_delta_is_one_additive_chunk_that_tombstones_nothing() {
        let fixture = generate(&FixtureSpec::new(account(), 10));
        let one = fixture.newest_first()[0].clone();
        let chunks = provider(Pass::Delta(vec![one])).chunks();

        assert_eq!(chunks.len(), 1);
        assert!(
            !chunks[0].is_reconcile_final(),
            "a delta must not tombstone the folder's other mail"
        );
        assert!(chunks[0].present.is_empty());
        assert_eq!(chunks[0].changed.len(), 1);
    }

    #[test]
    fn the_scopes_are_imap_shaped_so_each_folder_is_its_own_scope() {
        let inbox = provider(Pass::Delta(Vec::new()));
        let archive = FolderProvider::new(
            MailboxId::try_from("Archive").unwrap(),
            Vec::new(),
            Pass::Delta(Vec::new()),
        );
        assert_ne!(
            inbox.email_scope(&account()),
            archive.email_scope(&account()),
            "two folders must never share a mail scope, or their leases contend"
        );
        assert_eq!(
            inbox.mailbox_scope(&account()),
            archive.mailbox_scope(&account()),
            "the folder list is one shared scope, synced once"
        );
    }
}
