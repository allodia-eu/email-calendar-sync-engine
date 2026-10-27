//! A mail pass that outlasts its scope lease keeps it: a backfill paced under a server's quota
//! runs for as long as the mailbox is large, and a poll arriving partway must find the scope
//! held rather than take it over.

use engine_provider::{CalendarWrites, EmailChunk};
use engine_store::{LeaseRequest, StoreError, WorkerId};

use super::*;

/// The lease every pass here claims under.
const TTL: Duration = Duration::from_mins(1);

/// How far apart the chunks arrive, on the runtime's clock and the store's alike.
const EVERY: Duration = Duration::from_secs(5);

/// A backfill delivering one message every [`EVERY`] for `chunks` chunks, moving the store's
/// clock with the runtime's so a lease lapses in the store exactly when the pass has run past it.
struct SlowMail {
    mailboxes: Vec<Mailbox>,
    chunks: usize,
    clock: ManualClock,
}

#[async_trait::async_trait]
impl Provider for SlowMail {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Email,
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
            SyncState::new("mbox"),
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
        Box::pin(async_stream::try_stream! {
            for n in 0..self.chunks {
                tokio::time::sleep(EVERY).await;
                self.clock.advance(EVERY);
                let batch = vec![message(&format!("m{n}"), "a", "s")];
                yield EmailChunk::additive(batch, Vec::new(), None, SyncState::new(n.to_string()));
            }
        })
    }
}

impl engine_provider::MailboxWrites for SlowMail {}
impl CalendarWrites for SlowMail {}

#[tokio::test(start_paused = true)]
async fn a_pass_longer_than_its_lease_keeps_the_scope_to_itself() {
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    // Ten minutes of mail against a one-minute lease.
    let provider = SlowMail {
        mailboxes: vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        chunks: 120,
        clock: clock.clone(),
    };
    let account = account();
    let scope = provider.email_scope(&account);

    let pass = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account,
        worker(),
        TTL,
        StreamTuning::new(1, 1),
        &IgnoreCommits,
    );
    let rival = async {
        let mut refused = 0;
        // Past the first lease's expiry and up to the pass's last chunk.
        while refused < 80 {
            tokio::time::sleep(Duration::from_secs(7)).await;
            let claim = store
                .claim_sync_scope(
                    account.clone(),
                    &scope,
                    LeaseRequest::new(WorkerId::new("poll"), TTL),
                )
                .await;
            assert_eq!(
                claim.err(),
                Some(StoreError::ScopeHeld),
                "a poll took the scope {:?} into a pass that was still running",
                Duration::from_secs(7) * (refused + 1),
            );
            refused += 1;
        }
    };
    let (applied, ()) = tokio::join!(pass, rival);

    assert_eq!(applied.upserted(), 120);
    assert_eq!(store.object_keys(&scope).await.unwrap().len(), 120);
}

#[tokio::test(start_paused = true)]
async fn an_overdue_renewal_runs_even_when_the_next_chunk_is_already_there() {
    // A page split into many chunks hands each one over at once, so the time goes on the store's
    // work between them; a renewal that waited for the pass to wait on its provider never runs.
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let account = account();
    let scope = SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::Email,
    };
    let claim = store
        .claim_sync_scope(account.clone(), &scope, LeaseRequest::new(worker(), TTL))
        .await
        .unwrap();
    let mut keep_alive = crate::keep_alive::KeepAlive::new(&store, &claim.lease, TTL);

    // The store's work runs both clocks past the renewal's due time, and the next chunk is ready.
    tokio::time::advance(TTL / 2).await;
    clock.advance(TTL / 2);
    keep_alive.around(async {}).await;

    // Past the claim's own expiry, the scope is still the pass's.
    clock.advance(TTL / 2 + Duration::from_secs(1));
    let claim = store
        .claim_sync_scope(
            account,
            &scope,
            LeaseRequest::new(WorkerId::new("poll"), TTL),
        )
        .await;
    assert_eq!(claim.err(), Some(StoreError::ScopeHeld));
}
