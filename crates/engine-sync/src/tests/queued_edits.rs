//! A flag or read change is shown from the moment it is queued: through the throttled attempt,
//! through a sync that reports the server's older state, and out with the drain that finally
//! sends it. Only a refusal for good takes it back.

use engine_core::mail::{Keyword, SystemKeyword};

use super::*;
use crate::{MailEditSent, queue_mail_edit, send_mail_edit};

async fn sync<P: Provider>(provider: &P, store: &SqliteStore<ManualClock>) {
    sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
}

/// `(seen, flagged)` as the store shows message `m1`.
async fn shown(store: &SqliteStore<ManualClock>) -> (bool, bool) {
    let rows = store
        .list_mail(
            &[account()],
            engine_store::MailSelector::Keys(&[key("m1")]),
            usize::MAX,
        )
        .await
        .unwrap();
    (rows[0].mail.flags.seen(), rows[0].mail.flags.flagged())
}

fn inbox_with_one() -> FakeMail {
    FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    )
}

#[tokio::test]
async fn a_throttled_flag_stays_shown_through_a_sync_that_has_not_seen_it() {
    // The server throttles the edit, and meanwhile another client reads the message: the next
    // sync reports it read and unflagged.
    let provider = inbox_with_one()
        .failing(Fault::ThrottledEdit)
        .then_changing_state(vec![MailStateChange::keywords(
            key("m1"),
            [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
        )]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;

    let edit = MailEdit::set_flagged(key("m1"), true);
    let op = queue_mail_edit(&store, &account(), "flag:m1", &edit)
        .await
        .unwrap();
    assert_eq!(
        shown(&store).await,
        (false, true),
        "shown before it is sent"
    );

    let sent = send_mail_edit(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        op,
        &edit,
    )
    .await
    .unwrap();
    assert!(
        matches!(sent, MailEditSent::Queued { .. }),
        "a throttled edit is queued, not refused: {sent:?}"
    );
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Pending)
    );

    sync(&provider, &store).await;
    assert_eq!(
        shown(&store).await,
        (true, true),
        "the read state the server reported lands, and the flag still waiting stays"
    );
}

#[tokio::test]
async fn a_flag_the_server_refuses_for_good_goes_back() {
    let provider = inbox_with_one().failing(Fault::WriteGuard);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;

    let edit = MailEdit::set_flagged(key("m1"), true);
    let op = queue_mail_edit(&store, &account(), "flag:m1", &edit)
        .await
        .unwrap();
    assert_eq!(shown(&store).await, (false, true));

    let err = send_mail_edit(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        op,
        &edit,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::SyncError::Provider(_)), "{err:?}");
    assert_eq!(shown(&store).await, (false, false), "taken back");
}

#[tokio::test]
async fn a_flag_queued_offline_goes_out_with_the_drain() {
    let provider = inbox_with_one();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;

    // Queued with no attempt, as an edit made offline is.
    let edit = MailEdit::set_flagged(key("m1"), true);
    let op = queue_mail_edit(&store, &account(), "flag:m1", &edit)
        .await
        .unwrap();
    assert_eq!(shown(&store).await, (false, true));

    drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    assert_eq!(shown(&store).await, (false, true), "accepted, so it stays");
}
