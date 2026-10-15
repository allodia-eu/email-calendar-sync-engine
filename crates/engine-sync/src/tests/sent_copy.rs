//! A send awaiting confirmation, resolved by the copy a sync finds filed in Sent.

use engine_core::{
    mail::{Keyword, MailState},
    write::PendingOpId,
};
use engine_store::PendingOpClaim;

use super::*;

const SENT_UNDER: &str = "gen-unconfirmed@test.local";

/// Inbox, Sent and Drafts, each with its role.
fn mailboxes() -> Vec<Mailbox> {
    vec![
        mailbox("inbox", "Inbox", Some(MailboxRole::Inbox)),
        mailbox("sent", "Sent", Some(MailboxRole::Sent)),
        mailbox("drafts", "Drafts", Some(MailboxRole::Drafts)),
    ]
}

/// A copy of the send filed in `mailbox`, carrying the `Message-ID` it went out under.
fn copy_in(id: &str, mailbox: &str) -> Message {
    let mut copy = message(id, mailbox, "Quarterly report");
    copy.envelope.message_id = vec![MessageIdHeader::new(SENT_UNDER).unwrap()];
    copy
}

/// A send whose outcome is unknown, as a lost acknowledgement or an interrupted process leaves it.
async fn unconfirmed_send(store: &SqliteStore<ManualClock>) -> PendingOpId {
    let op = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(format!("submit:{SENT_UNDER}")).unwrap(),
                PendingOpKind::MailSubmit,
                ResourceKey::new(format!("draft:{SENT_UNDER}")).unwrap(),
                serde_json::to_value(draft(SENT_UNDER)).unwrap(),
            ),
        )
        .await
        .unwrap();
    let PendingOpClaim::Leased(leased) = store
        .claim_pending_op(
            account(),
            op,
            LeaseRequest::new(worker(), Duration::from_mins(5)),
        )
        .await
        .unwrap()
    else {
        panic!("a fresh op is claimable");
    };
    store
        .mark_pending_op(
            &leased.lease,
            PendingOutcome::NeedsConfirmation {
                detail: "the acknowledgement after DATA never arrived".to_owned(),
            },
        )
        .await
        .unwrap();
    op
}

async fn sync(provider: &FakeMail, store: &SqliteStore<ManualClock>, tuning: StreamTuning) {
    sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        tuning,
        &IgnoreCommits,
    )
    .await;
}

#[tokio::test]
async fn the_copy_filed_in_sent_confirms_the_send() {
    for tuning in [StreamTuning::new(0, 0), StreamTuning::responsive()] {
        let store = SqliteStore::open_in_memory(clock()).unwrap();
        let op = unconfirmed_send(&store).await;
        let provider = FakeMail::new(mailboxes(), vec![copy_in("m-sent", "sent")]);

        sync(&provider, &store, tuning).await;

        assert_eq!(
            store.pending_op_state(op).await.unwrap(),
            Some(PendingOpState::Succeeded),
            "the server filed it in Sent, so it went"
        );
    }
}

/// A draft saved to the server carries the same `Message-ID`, and says nothing about delivery.
#[tokio::test]
async fn a_copy_in_drafts_confirms_nothing() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = unconfirmed_send(&store).await;
    let provider = FakeMail::new(mailboxes(), vec![copy_in("m-draft", "drafts")]);

    sync(&provider, &store, StreamTuning::new(0, 0)).await;

    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
}

/// JMAP and Gmail send by moving the draft into Sent, so the proof arrives as a state change on
/// a key the store already holds, and the `Message-ID` is in its stored payload.
#[tokio::test]
async fn a_draft_moved_into_sent_confirms_the_send() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = unconfirmed_send(&store).await;
    let moved = MailStateChange::new(
        key("m-draft"),
        MailState::with_keywords(BTreeSet::<Keyword>::new())
            .filed_in(Memberships::of_one(MailboxId::try_from("sent").unwrap())),
    );
    let provider = FakeMail::new(mailboxes(), vec![copy_in("m-draft", "drafts")])
        .then_changing_state(vec![moved]);

    sync(&provider, &store, StreamTuning::new(0, 0)).await;
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation),
        "still only a draft"
    );
    // Second pass: the cursor exists, so the fake emits the move.
    sync(&provider, &store, StreamTuning::new(0, 0)).await;

    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// A send that has not been attempted, or is being attempted now, is not this module's to
/// resolve: its own worker records what happens.
#[tokio::test]
async fn a_queued_send_is_left_to_its_worker() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let op = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new(format!("submit:{SENT_UNDER}")).unwrap(),
                PendingOpKind::MailSubmit,
                ResourceKey::new(format!("draft:{SENT_UNDER}")).unwrap(),
                serde_json::to_value(draft(SENT_UNDER)).unwrap(),
            ),
        )
        .await
        .unwrap();
    let provider = FakeMail::new(mailboxes(), vec![copy_in("m-sent", "sent")]);

    sync(&provider, &store, StreamTuning::new(0, 0)).await;

    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Pending)
    );
}
