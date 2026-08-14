//! A mark-read moves the flag and leaves everything else where it was.
//!
//! An apply used to write the message the provider sent over the whole stored payload, and two
//! of that payload's fields are the engine's, not the provider's: the `thread` derivation
//! assigned, and the `preview` IMAP has no server snippet for. A flag change arrived as a
//! re-mapped message carrying neither, so marking one message read dropped it out of its
//! conversation and blanked its list row.
//!
//! A keyword change is no longer a message. It names the keywords and nothing else, so there is
//! nothing left for it to destroy — and the derivation pass that runs after it must not undo it
//! either, which is the second test here.
//!
//! Both drivers are exercised: `sync_mail` (whole scope) and `sync_mail_streamed` (per chunk).
//! They build their batches in different places, so a fix applied to one is not a fix.

use engine_core::{
    ids::ThreadId,
    mail::{Keyword, SystemKeyword, ThreadRef},
};

use super::*;

/// The message as it is first stored: threaded, with a snippet, and unread.
fn threaded(subject: &str) -> Message {
    let mut message = message("m1", "a", subject);
    message.thread = Some(ThreadRef::derived(
        ThreadId::try_from("thread-root").unwrap(),
    ));
    message.preview = Some("The numbers you asked for are attached.".to_owned());
    message
}

/// The change a mark-read produces: `$seen`, and no claim about anything else.
fn marked_seen() -> MailKeywordChange {
    MailKeywordChange::new(
        key("m1"),
        [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
    )
}

/// The stored row a list is built from.
async fn listed(store: &SqliteStore<ManualClock>) -> engine_store::MailListRow {
    store
        .list_mail(
            &[account()],
            engine_store::MailSelector::Keys(&[key("m1")]),
            usize::MAX,
        )
        .await
        .unwrap()
        .pop()
        .expect("the message is listed")
}

/// Asserts the row carries the new flag and every field the change never mentioned.
fn intact_and_seen(row: &engine_store::MailListRow, thread: &str) {
    assert!(row.mail.flags.seen(), "the flag the change carried moved");
    assert_eq!(
        row.mail.thread_id.as_ref().map(ThreadId::as_str),
        Some(thread),
        "the message stays in its conversation — this is the list-jumping bug"
    );
    assert_eq!(
        row.mail.preview.as_deref(),
        Some("The numbers you asked for are attached."),
        "the snippet survives, so the row does not go blank"
    );
    assert_eq!(row.mail.subject.as_deref(), Some("Quarterly report"));
}

#[tokio::test]
async fn a_keyword_change_keeps_the_thread_and_preview_whole_scope() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![threaded("Quarterly report")],
    )
    .then_marking(vec![marked_seen()]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert!(!listed(&store).await.mail.flags.seen(), "starts unread");

    // Second pass: the cursor exists, so the fake emits the armed keyword change.
    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    intact_and_seen(&listed(&store).await, "thread-root");
}

#[tokio::test]
async fn a_keyword_change_keeps_the_thread_and_preview_streamed() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![threaded("Quarterly report")],
    )
    .then_marking(vec![marked_seen()]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    for _ in 0..2 {
        sync_mail_streamed(
            &provider,
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
            StreamTuning::responsive(),
            &IgnoreCommits,
        )
        .await
        .unwrap();
    }
    intact_and_seen(&listed(&store).await, "thread-root");
}

/// The derivation pass runs after every sync. It reads payloads to rebuild the reference graph,
/// and the payload of a message whose keywords moved still carries the **old** ones — so a pass
/// that re-projected whole messages from what it read would hand the flag straight back.
///
/// It writes thread ids alone, which is why it cannot.
#[tokio::test]
async fn the_derivation_pass_does_not_hand_back_a_flag_a_keyword_change_moved() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![threaded("Quarterly report")],
    )
    .then_marking(vec![marked_seen()]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    // Let the grouping settle first: this message owns no `Message-ID`, so the pass re-keys its
    // seeded thread to the fallback (its provider key) once, legitimately. The question here is
    // what a *later* pass does to a flag, not what the first one does to a thread.
    crate::derive_mail_threads(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    let settled = listed(&store)
        .await
        .mail
        .thread_id
        .expect("the pass assigned a thread");

    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert!(
        listed(&store).await.mail.flags.seen(),
        "the mark-read landed"
    );

    crate::derive_mail_threads(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    intact_and_seen(&listed(&store).await, settled.as_str());
}

/// A second derivation pass over unchanged mail assigns nothing.
///
/// The pass compares its computed assignment against the **stored row**, not against the
/// payload it rebuilt the graph from. Comparing against the payload would re-assign every
/// message on every pass, forever, because a thread-only write leaves the payload alone.
#[tokio::test]
async fn a_repeat_derivation_pass_writes_nothing() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Quarterly report")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    let first = crate::derive_mail_threads(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    assert_eq!(
        first.messages_assigned, 1,
        "the message had no thread id and gets one"
    );
    let second = crate::derive_mail_threads(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    assert_eq!(
        second.messages_assigned, 0,
        "nothing moved, so the pass converges instead of rewriting every message"
    );
}
