//! A sent copy's keywords on an adapter that keeps them after the send: the send's answer does
//! not wait for the copy, and the outbox records the edit that tags it for the drainer.

use engine_core::mail::Keyword;
use engine_store::PendingOpRow;

use super::*;
use crate::SubmitOutcome;

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

fn tagged(message_id: &str) -> Draft {
    draft(message_id).with_sent_copy_keyword(keyword())
}

async fn send(provider: &FakeMail, store: &SqliteStore<ManualClock>, id: &str) -> SubmitOutcome {
    submit_mail(
        provider,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &tagged(id),
    )
    .await
    .unwrap()
}

/// The ops still waiting, other than the send itself.
async fn follow_ups(store: &SqliteStore<ManualClock>) -> Vec<PendingOpRow> {
    store
        .list_pending_ops(account())
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.kind == Some(PendingOpKind::MailEdit))
        .collect()
}

#[tokio::test]
async fn a_send_records_the_edit_that_keeps_its_keywords_and_reports_none_kept() {
    let provider = FakeMail::new(vec![], vec![]).keeping_keywords_after_the_send();
    let (store, _) = drain::store_and_clock();

    let outcome = send(&provider, &store, "tag-1@test.local").await;

    assert!(
        outcome.sent_copy_keywords.is_empty(),
        "nothing reads as kept before the edit lands"
    );
    let queued = follow_ups(&store).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].state, PendingOpState::Pending);
    let edit: MailEdit = serde_json::from_value(queued[0].payload.clone()).unwrap();
    assert_eq!(
        edit,
        MailEdit::SetKeywords {
            target: outcome.email_key.clone(),
            add: [keyword()].into(),
            remove: BTreeSet::new(),
        }
    );
}

#[tokio::test]
async fn an_adapter_that_keeps_them_within_the_send_records_no_edit() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _) = drain::store_and_clock();

    send(&provider, &store, "tag-2@test.local").await;

    assert!(follow_ups(&store).await.is_empty());
}

#[tokio::test]
async fn a_draft_asking_for_no_keyword_records_no_edit() {
    let provider = FakeMail::new(vec![], vec![]).keeping_keywords_after_the_send();
    let (store, _) = drain::store_and_clock();

    submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("tag-3@test.local"),
    )
    .await
    .unwrap();

    assert!(follow_ups(&store).await.is_empty());
}

#[tokio::test]
async fn the_drainer_applies_the_recorded_edit() {
    let provider = FakeMail::new(vec![], vec![]).keeping_keywords_after_the_send();
    let (store, _) = drain::store_and_clock();
    send(&provider, &store, "tag-4@test.local").await;

    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(report.attempted.len(), 1);
    assert_eq!(report.attempted[0].outcome, DrainOutcome::Succeeded);
    assert!(
        follow_ups(&store).await.is_empty(),
        "nothing is left waiting"
    );
}

/// A send the drainer delivers later records the edit too: the queued path is the one an
/// offline send takes, and it must not lose the keywords the inline path keeps.
#[tokio::test]
async fn a_queued_send_delivered_by_the_drainer_records_the_edit_too() {
    let provider = FakeMail::new(vec![], vec![])
        .keeping_keywords_after_the_send()
        .failing_sends(1);
    let (store, clock) = drain::store_and_clock();
    submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &tagged("tag-5@test.local"),
    )
    .await
    .expect_err("the first attempt is refused and the send stays queued");
    clock.advance(Duration::from_mins(1));

    drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(follow_ups(&store).await.len(), 1);
}
