//! The keywords a stored draft carries, through the outbox: the inline save reports what was
//! kept, a save queued with no network takes its keywords to the drainer, and a change of
//! keywords alone is a save of its own.

use super::*;
use crate::outbox::{DraftPut, put_draft_mail};

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

fn composing(id: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "body",
    )
}

fn store_and_clock() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = clock();
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

async fn save(
    provider: &FakeMail,
    store: &SqliteStore<ManualClock>,
    draft: &Draft,
    keywords: &BTreeSet<Keyword>,
) -> Result<crate::outbox::PutDraftOutcome, crate::SyncError> {
    put_draft_mail(
        provider,
        store,
        &account(),
        WorkerId::new("w"),
        Duration::from_secs(30),
        draft,
        None,
        keywords,
    )
    .await
}

#[tokio::test]
async fn a_save_hands_its_keywords_over_and_reports_what_was_kept() {
    let provider = FakeMail::new(vec![], vec![]).keeping_draft_keywords();
    let (store, _clock) = store_and_clock();
    let wanted = BTreeSet::from([keyword()]);

    let saved = save(&provider, &store, &composing("kw-1@test.local"), &wanted)
        .await
        .unwrap();

    assert_eq!(saved.keywords, wanted);
    assert_eq!(provider.draft_puts.lock().unwrap()[0].1, wanted);
}

#[tokio::test]
async fn an_adapter_that_keeps_none_says_so_and_the_save_still_succeeds() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();

    let saved = save(
        &provider,
        &store,
        &composing("kw-2@test.local"),
        &BTreeSet::from([keyword()]),
    )
    .await
    .unwrap();

    assert_eq!(saved.keywords, BTreeSet::new());
    assert_eq!(
        store.pending_op_state(saved.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_save_queued_with_no_network_takes_its_keywords_to_the_drainer() {
    let provider = FakeMail::new(vec![], vec![])
        .keeping_draft_keywords()
        .failing_drafts(1);
    let (store, clock) = store_and_clock();
    let wanted = BTreeSet::from([keyword()]);

    save(&provider, &store, &composing("kw-3@test.local"), &wanted)
        .await
        .expect_err("the save had no network");
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
    assert_eq!(provider.draft_puts.lock().unwrap()[0].1, wanted);
}

#[tokio::test]
async fn changing_only_the_keywords_is_a_save_of_its_own() {
    // The op is named by what it stores, and the keywords are part of that: a save that only
    // adds one must reach the server, not land on the op the unchanged text left behind.
    let provider = FakeMail::new(vec![], vec![]).keeping_draft_keywords();
    let (store, _clock) = store_and_clock();
    let draft = composing("kw-4@test.local");

    let plain = save(&provider, &store, &draft, &BTreeSet::new())
        .await
        .unwrap();
    let tagged = save(&provider, &store, &draft, &BTreeSet::from([keyword()]))
        .await
        .unwrap();

    assert_ne!(plain.op, tagged.op);
    assert_eq!(provider.draft_puts.lock().unwrap().len(), 2);
    assert_eq!(tagged.keywords, BTreeSet::from([keyword()]));
}

#[test]
fn a_queued_save_from_before_keywords_still_reads() {
    let draft = composing("kw-5@test.local");
    let old = serde_json::json!({ "draft": draft, "replacing": null });
    let read: DraftPut = serde_json::from_value(old).unwrap();
    assert_eq!(read.keywords, BTreeSet::new());
}
