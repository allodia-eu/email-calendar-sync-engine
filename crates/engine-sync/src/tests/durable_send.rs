//! A send against a process that ends, stalls or is overtaken at any point of the attempt.
//!
//! [`Scripted`] is a provider whose submissions follow a script, counting the messages that
//! actually reach the server: one counts the moment the attempt writes past its hand-over.
//! "The process ends" is the attempt's future being dropped: nothing it would have written
//! afterwards is written, which is what the store sees of a process that was killed.

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

use engine_provider::HandOver;
use engine_store::{OpRejection, PendingOpClaim};
use tokio::sync::Notify;

use super::*;

/// What the next submission does.
enum Step {
    /// Hands over and is accepted.
    Deliver,
    /// Stops before the hand-over for good: the process ends while connecting.
    DieBeforeHandOver,
    /// Hands over, reaches the server, and stops before the answer for good.
    DieAfterHandOver,
    /// Takes `ticks` × `every` of both clocks before it hands over and is accepted: a large
    /// upload on a slow line.
    Slow { ticks: u32, every: Duration },
    /// Says it reached the hand-over, then waits to be told to go on.
    Pause(Arc<Notify>, Arc<Notify>),
}

struct Scripted {
    inner: FakeMail,
    clock: ManualClock,
    steps: Mutex<VecDeque<Step>>,
    /// Messages that reached the server.
    delivered: AtomicUsize,
}

impl Scripted {
    fn new(clock: &ManualClock, steps: Vec<Step>) -> Self {
        Self::over(FakeMail::new(vec![], vec![]), clock, steps)
    }

    fn over(inner: FakeMail, clock: &ManualClock, steps: Vec<Step>) -> Self {
        Self {
            inner,
            clock: clock.clone(),
            steps: Mutex::new(steps.into()),
            delivered: AtomicUsize::new(0),
        }
    }

    fn delivered(&self) -> usize {
        self.delivered.load(Ordering::SeqCst)
    }

    /// The irreversible step: only reachable with the hand-over recorded.
    async fn deliver(
        &self,
        draft: &Draft,
        hand_over: &HandOver<'_>,
    ) -> ProviderResult<SubmissionReceipt> {
        let proof = hand_over.commit().await?;
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(SubmissionReceipt::filed(
            key("sent-1"),
            draft.message_id.clone(),
            &proof,
        ))
    }
}

impl engine_provider::MailboxWrites for Scripted {}
impl CalendarWrites for Scripted {}

#[async_trait::async_trait]
impl Provider for Scripted {
    fn connection_info(&self) -> ConnectionInfo {
        self.inner.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.email_scope(account)
    }

    async fn sync_mailboxes(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        self.inner.sync_mailboxes(account, cursor).await
    }

    fn stream_email<'a>(
        &'a self,
        account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        self.inner
            .stream_email(account, cursor, window, fetch_batch, chunk_size)
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
        hand_over: &HandOver<'_>,
    ) -> ProviderResult<SubmissionReceipt> {
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Step::Deliver);
        match step {
            Step::Deliver => self.deliver(draft, hand_over).await,
            Step::DieBeforeHandOver => std::future::pending().await,
            Step::DieAfterHandOver => {
                self.deliver(draft, hand_over).await?;
                std::future::pending().await
            }
            Step::Slow { ticks, every } => {
                for _ in 0..ticks {
                    self.clock.advance(every);
                    tokio::time::sleep(every).await;
                }
                self.deliver(draft, hand_over).await
            }
            Step::Pause(reached, go) => {
                reached.notify_one();
                go.notified().await;
                self.deliver(draft, hand_over).await
            }
        }
    }
}

const TTL: Duration = Duration::from_mins(1);

/// Sends `id` until `die` resolves, as a process the OS ends at that moment.
async fn send_until<F: Future<Output = ()>>(
    provider: &Scripted,
    store: &SqliteStore<ManualClock>,
    id: &str,
    die: F,
) {
    let (acct, message) = (account(), draft(id));
    tokio::select! {
        biased;
        () = die => {}
        sent = submit_mail(provider, store, &acct, worker(), TTL, &message) => {
            panic!("the attempt was meant to be cut off, and ended: {sent:?}");
        }
    }
}

/// Lets the attempt run as far as it can get without the clock moving, then ends it.
async fn send_and_die(provider: &Scripted, store: &SqliteStore<ManualClock>, id: &str) {
    send_until(provider, store, id, async {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

async fn drain(provider: &Scripted, store: &SqliteStore<ManualClock>) -> crate::DrainReport {
    drain_outbox(provider, store, &account(), WorkerId::new("w-2"), TTL)
        .await
        .unwrap()
}

async fn only_row(store: &SqliteStore<ManualClock>) -> engine_store::PendingOpRow {
    let mut rows = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows.remove(0)
}

#[tokio::test]
async fn a_send_cut_off_before_its_hand_over_goes_out_once_with_the_next_drain() {
    let (store, clock) = drain::store_and_clock();
    let provider = Scripted::new(&clock, vec![Step::DieBeforeHandOver]);
    send_and_die(&provider, &store, "cut-connecting@test.local").await;
    assert_eq!(provider.delivered(), 0);

    // Nothing durable says it went, so once the lease lapses it is an ordinary retry.
    clock.advance(TTL * 2);
    assert_eq!(only_row(&store).await.state, PendingOpState::Pending);
    let report = drain(&provider, &store).await;
    assert_eq!(report.delivered(), 1);
    assert_eq!(provider.delivered(), 1);
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_send_cut_off_after_its_hand_over_is_never_sent_again() {
    let (store, clock) = drain::store_and_clock();
    let provider = Scripted::new(&clock, vec![Step::DieAfterHandOver]);
    send_and_die(&provider, &store, "cut-after@test.local").await;
    assert_eq!(provider.delivered(), 1);

    clock.advance(TTL * 2);
    let row = only_row(&store).await;
    assert_eq!(row.state, PendingOpState::NeedsConfirmation);
    assert!(drain(&provider, &store).await.attempted.is_empty());
    assert_eq!(
        store.retry_pending_op_now(account(), row.id).await.unwrap(),
        Some(OpRejection::AwaitingConfirmation)
    );
    assert!(drain(&provider, &store).await.attempted.is_empty());
    // Sending the same message again names the same op, which is not run again.
    assert!(
        submit_mail(
            &provider,
            &store,
            &account(),
            worker(),
            TTL,
            &draft("cut-after@test.local")
        )
        .await
        .is_err()
    );
    assert_eq!(provider.delivered(), 1, "the recipients have it once");
}

/// "Send now" on a send whose process died before it handed over actually sends it, at once,
/// rather than leaving it in flight under a worker that is gone.
#[tokio::test]
async fn send_now_on_a_dead_attempt_that_never_handed_over_sends_it() {
    let (store, clock) = drain::store_and_clock();
    let provider = Scripted::new(&clock, vec![Step::DieBeforeHandOver]);
    send_and_die(&provider, &store, "cut-hurried@test.local").await;
    clock.advance(TTL * 2);

    let op = only_row(&store).await.id;
    assert_eq!(
        store.retry_pending_op_now(account(), op).await.unwrap(),
        None
    );
    assert_eq!(drain(&provider, &store).await.delivered(), 1);
    assert_eq!(provider.delivered(), 1);
}

/// An attempt that outlives its lease keeps it by renewing, so neither a drain nor a claim from
/// another worker takes the op while it runs, and the renewal stops when the attempt does.
#[tokio::test(start_paused = true)]
async fn a_slow_send_keeps_its_lease_and_is_attempted_once() {
    let (store, clock) = drain::store_and_clock();
    // Ten minutes against a one-minute lease.
    let every = Duration::from_secs(5);
    let provider = Scripted::new(&clock, vec![Step::Slow { ticks: 120, every }]);
    let rival = Scripted::new(&clock, vec![]);

    let (acct, message) = (account(), draft("slow@test.local"));
    let sending = submit_mail(&provider, &store, &acct, worker(), TTL, &message);
    let contending = async {
        let mut tries = 0;
        while store.list_pending_ops(account()).await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        while provider.delivered() == 0 {
            tokio::time::sleep(Duration::from_secs(7)).await;
            assert!(drain(&rival, &store).await.attempted.is_empty());
            let Some(op) = store
                .list_pending_ops(account())
                .await
                .unwrap()
                .first()
                .map(|r| r.id)
            else {
                break;
            };
            let claim = store
                .claim_pending_op(
                    account(),
                    op,
                    LeaseRequest::new(WorkerId::new("rival"), TTL),
                )
                .await
                .unwrap();
            if provider.delivered() == 0 {
                assert_eq!(
                    claim,
                    PendingOpClaim::Refused(engine_store::ClaimRejection::Busy)
                );
            }
            tries += 1;
        }
        tries
    };
    let (sent, tries) = tokio::join!(sending, contending);
    sent.unwrap();
    assert!(tries > 60, "contended for the whole ten minutes: {tries}");
    assert_eq!(provider.delivered(), 1);
    assert_eq!(rival.delivered(), 0);
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}

/// The heartbeat itself: it renews while the work runs and stops with it, so a lease outlives
/// its work by no more than its TTL.
#[tokio::test(start_paused = true)]
async fn the_lease_is_renewed_while_the_work_runs_and_not_after() {
    let (store, clock) = drain::store_and_clock();
    let op = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit-held").unwrap(),
                PendingOpKind::MailEdit,
                ResourceKey::new("mail:held").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    let PendingOpClaim::Leased(leased) = store
        .claim_pending_op(account(), op, LeaseRequest::new(worker(), TTL))
        .await
        .unwrap()
    else {
        panic!("claimable");
    };
    let rival = || async {
        store
            .claim_pending_op(
                account(),
                op,
                LeaseRequest::new(WorkerId::new("rival"), TTL),
            )
            .await
            .unwrap()
    };

    let work = async {
        for _ in 0..20 {
            clock.advance(Duration::from_secs(15));
            tokio::time::sleep(Duration::from_secs(15)).await;
            assert_eq!(
                rival().await,
                PendingOpClaim::Refused(engine_store::ClaimRejection::Busy)
            );
        }
    };
    crate::outbox::holding(&store, &leased.lease, TTL, work).await;

    // The work has ended without recording anything. Were a renewal still running, the lease
    // would stay live however long this waits.
    tokio::time::sleep(TTL * 10).await;
    clock.advance(TTL + Duration::from_secs(1));
    assert_ne!(
        rival().await,
        PendingOpClaim::Refused(engine_store::ClaimRejection::Busy),
        "the lease lapsed once the work ended"
    );
}

/// The hand-over cannot be recorded because another process recovered the op first (its
/// start-up recovery, with this attempt still running): the attempt stops short of the server,
/// and the next drain sends the message once.
#[tokio::test]
async fn an_attempt_that_cannot_record_its_hand_over_never_reaches_the_server() {
    let (store, clock) = drain::store_and_clock();
    let (reached, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let provider = Scripted::new(
        &clock,
        vec![Step::Pause(Arc::clone(&reached), Arc::clone(&go))],
    );

    let (acct, message) = (account(), draft("overtaken@test.local"));
    let sending = submit_mail(&provider, &store, &acct, worker(), TTL, &message);
    let overtaking = async {
        reached.notified().await;
        assert_eq!(store.recover_interrupted_ops().await.unwrap(), 1);
        go.notify_one();
    };
    let (sent, ()) = tokio::join!(sending, overtaking);
    let err = sent.expect_err("the attempt stopped before the server");
    assert!(err.to_string().contains("could not record"), "{err}");
    assert_eq!(provider.delivered(), 0);

    assert_eq!(only_row(&store).await.state, PendingOpState::Pending);
    assert_eq!(drain(&provider, &store).await.delivered(), 1);
    assert_eq!(provider.delivered(), 1);
}

/// A send recovered as possibly delivered is resolved by its copy in Sent, on a server that
/// files it there, and the drainer never sends it again.
#[tokio::test]
async fn a_send_cut_off_after_its_hand_over_is_confirmed_by_its_copy_in_sent() {
    let (store, clock) = drain::store_and_clock();
    let sent_under = "filed-after-cut@test.local";
    let mut copy = message("m-sent", "sent", "Subject");
    copy.envelope.message_id = vec![MessageIdHeader::new(sent_under).unwrap()];
    let mailboxes = vec![
        mailbox("inbox", "Inbox", Some(MailboxRole::Inbox)),
        mailbox("sent", "Sent", Some(MailboxRole::Sent)),
    ];
    let provider = Scripted::over(
        FakeMail::new(mailboxes, vec![copy]),
        &clock,
        vec![Step::DieAfterHandOver],
    );
    send_and_die(&provider, &store, sent_under).await;

    // The next start-up.
    assert_eq!(store.recover_interrupted_ops().await.unwrap(), 1);
    let op = only_row(&store).await.id;
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
    sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        TTL,
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;

    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    assert!(drain(&provider, &store).await.attempted.is_empty());
    assert_eq!(provider.delivered(), 1);
}
