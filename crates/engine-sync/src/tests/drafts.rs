//! Keeping a draft on the server through the outbox: the inline save, what a save with
//! no network leaves behind, and what the drainer then does with it.

use super::*;
use crate::outbox::{delete_draft_mail, put_draft_mail};

/// A draft of the composition `id`, as a composer would hand it over.
fn composing(id: &str, subject: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        subject,
        "body",
    )
}

fn store_and_clock() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = clock();
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

#[tokio::test]
async fn a_save_returns_the_key_to_keep() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();

    let saved = put_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        &composing("compose-1@test.local", "v1"),
        None,
    )
    .await
    .unwrap();

    // The key is the provider's answer, not the caller's guess: three of the four
    // adapters move it on a re-save.
    assert_eq!(saved.key.as_str(), "draft-1");
    assert_eq!(
        store.pending_op_state(saved.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_save_with_no_network_is_kept_and_drained_later() {
    // The case the whole outbox exists for: the user saves, there is no network, and the
    // draft must still reach the server when there is.
    let provider = FakeMail::new(vec![], vec![]).failing_drafts(1);
    let (store, clock) = store_and_clock();

    let offline = put_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        &composing("compose-2@test.local", "v1"),
        None,
    )
    .await;
    assert!(offline.is_err(), "the save had no network");
    assert!(provider.draft_puts.lock().unwrap().is_empty());

    // A retryable failure parks the op, so it is not due until its backoff elapses.
    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        WorkerId::new("drainer"),
        Duration::from_secs(30),
    )
    .await
    .unwrap();

    assert_eq!(report.delivered(), 1, "the queued save did not drain");
    assert_eq!(provider.draft_puts.lock().unwrap().len(), 1);

    // And the caller learns what the draft is stored under. Nothing else can tell it: the
    // op settled with nobody watching, and a settled op is not in the queue read. Without
    // this the next save cannot name the copy it supersedes and stores a second one.
    assert_eq!(
        report.attempted[0]
            .provider_key
            .as_ref()
            .map(ProviderKey::as_str),
        Some("draft-1")
    );
}

#[tokio::test]
async fn saving_again_with_no_network_replaces_what_was_queued() {
    // Five saves offline must not become five writes against the user's Drafts folder.
    // Only the latest text is worth anything; the ones before it are superseded.
    let provider = FakeMail::new(vec![], vec![]).failing_drafts(5);
    let (store, clock) = store_and_clock();

    for version in ["v1", "v2", "v3", "v4", "v5"] {
        let _ = put_draft_mail(
            &provider,
            &store,
            &account(),
            WorkerId::new("w"),
            Duration::from_secs(30),
            &composing("compose-3@test.local", version),
            None,
        )
        .await;
    }

    let queued: Vec<_> = store
        .list_pending_ops(account())
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.kind == Some(PendingOpKind::MailDraftPut))
        .collect();
    assert_eq!(queued.len(), 1, "every save queued a write of its own");

    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        WorkerId::new("drainer"),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(report.delivered(), 1);

    // And it is the newest text that reached the server, not the first.
    let puts = provider.draft_puts.lock().unwrap();
    assert_eq!(puts.len(), 1);
}

#[tokio::test]
async fn saving_the_same_text_again_queues_nothing_new() {
    // Nothing changed between the two, so the idempotency key is the same and the second
    // save lands on the op the first one left queued rather than adding a write beside it.
    // It is refused while that op serves its backoff, which is the honest answer: what the
    // user asked for is already queued, carrying exactly this text.
    let provider = FakeMail::new(vec![], vec![]).failing_drafts(2);
    let (store, _clock) = store_and_clock();
    let draft = composing("compose-4@test.local", "v1");

    let first = put_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        &draft,
        None,
    )
    .await;
    assert!(first.is_err(), "the save had no network");

    let second = put_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        &draft,
        None,
    )
    .await;
    assert!(second.is_err());

    let queued: Vec<_> = store
        .list_pending_ops(account())
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.kind == Some(PendingOpKind::MailDraftPut))
        .collect();
    assert_eq!(queued.len(), 1, "an identical save queued a second write");
    // One provider attempt: the second save never reached it, so the outage is not
    // burned through the op's attempt budget by a user pressing save twice.
    assert_eq!(queued[0].attempts, 1);
}

#[tokio::test]
async fn a_removal_drains_like_a_save() {
    let provider = FakeMail::new(vec![], vec![]).failing_drafts(1);
    let (store, clock) = store_and_clock();
    let key = ProviderKey::new("draft-9").unwrap();

    let offline = delete_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        &key,
    )
    .await;
    assert!(offline.is_err());

    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        WorkerId::new("drainer"),
        Duration::from_secs(30),
    )
    .await
    .unwrap();

    assert_eq!(report.delivered(), 1);
    assert_eq!(*provider.draft_deletes.lock().unwrap(), vec![key]);
}

#[tokio::test]
async fn a_re_save_names_the_copy_it_supersedes() {
    // Online, the caller keeps what the first save returned and hands it back. That is
    // the contract the key exists for, and on three of the four adapters the second save
    // resolves to a different key again.
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    let save = |draft: Draft, replacing: Option<ProviderKey>| {
        let provider = &provider;
        let store = &store;
        async move {
            put_draft_mail(
                provider,
                store,
                &account(),
                WorkerId::new("w"),
                Duration::from_secs(30),
                &draft,
                replacing.as_ref(),
            )
            .await
            .unwrap()
        }
    };

    let first = save(composing("compose-5@test.local", "v1"), None).await;
    let second = save(
        composing("compose-5@test.local", "v2"),
        Some(first.key.clone()),
    )
    .await;

    assert_ne!(
        first.key, second.key,
        "the key moved, as it must be allowed to"
    );
    assert_eq!(provider.draft_puts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_removal_that_succeeds_settles_its_op() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    let key = ProviderKey::new("draft-7").unwrap();

    let op = delete_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        &key,
    )
    .await
    .unwrap();

    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    assert_eq!(*provider.draft_deletes.lock().unwrap(), vec![key]);
}
