//! A sync must not destroy what the engine derived.
//!
//! An apply writes the message the provider sent over the whole stored payload, and
//! two of that payload's fields are the engine's, not the provider's: the `thread`
//! derivation assigned, and the `preview` IMAP has no server snippet for. A flag
//! change arrives as a re-mapped message carrying neither — so without the restore in
//! `derived.rs`, marking one message read dropped it out of its conversation and
//! blanked its list row until the next full derivation pass.
//!
//! Both drivers are exercised: `sync_mail` (whole scope) and `sync_mail_streamed`
//! (per chunk). They build their updates in different places, so a fix applied to one
//! is not a fix.

use engine_core::{
    ids::ThreadId,
    mail::{Keyword, SystemKeyword, ThreadRef},
};

use super::*;

/// The message as it is first stored: threaded and with a snippet.
fn threaded(subject: &str) -> Message {
    let mut message = message("m1", "a", subject);
    message.thread = Some(ThreadRef::derived(
        ThreadId::try_from("thread-root").unwrap(),
    ));
    message.preview = Some("The numbers you asked for are attached.".to_owned());
    message
}

/// The same message as a flag change re-maps it: `$seen` set, and — because the
/// provider has no way to know either — no thread and no preview.
fn flag_only() -> Message {
    let mut message = message("m1", "a", "Quarterly report");
    message
        .keywords
        .insert(Keyword::system(SystemKeyword::Seen));
    assert!(message.thread.is_none() && message.preview.is_none());
    message
}

/// The stored message, and the thread its index row names.
async fn stored(
    store: &SqliteStore<ManualClock>,
    scope: &SyncScope,
) -> (Message, Option<ThreadId>) {
    let payload = store
        .object_payload(scope, &key("m1"))
        .await
        .unwrap()
        .expect("the message is stored");
    let indexed = store
        .scope_mail_index(scope)
        .await
        .unwrap()
        .into_iter()
        .find(|(entry, ..)| entry == &key("m1"))
        .expect("the message has an index row")
        .2;
    (serde_json::from_value(payload).unwrap(), indexed)
}

/// Asserts a flag-only delta left the derived fields — and the index row that groups
/// the list — exactly as the first pass stored them.
async fn assert_survived(store: &SqliteStore<ManualClock>, scope: &SyncScope) {
    let (message, indexed) = stored(store, scope).await;
    assert!(
        message
            .keywords
            .contains(&Keyword::system(SystemKeyword::Seen)),
        "the flag change itself must land — otherwise this test proves nothing"
    );
    assert_eq!(
        message.thread.as_ref().map(|thread| &thread.id),
        Some(&ThreadId::try_from("thread-root").unwrap()),
        "the message must stay in its conversation across a flag change"
    );
    assert_eq!(
        message.preview.as_deref(),
        Some("The numbers you asked for are attached."),
        "the list row must keep its snippet"
    );
    assert_eq!(
        indexed,
        Some(ThreadId::try_from("thread-root").unwrap()),
        "the index row groups the list, so it must carry the thread too"
    );
}

#[tokio::test]
async fn a_flag_only_delta_keeps_the_derived_thread_and_preview() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![threaded("Quarterly report")],
    )
    .then_changing(vec![flag_only()]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let scope = provider.email_scope(&account());

    for _ in 0..2 {
        sync_mail(
            &provider,
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
        )
        .await
        .unwrap();
    }
    assert_survived(&store, &scope).await;
}

#[tokio::test]
async fn a_streamed_flag_only_chunk_keeps_the_derived_thread_and_preview() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![threaded("Quarterly report")],
    )
    .then_changing(vec![flag_only()]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let scope = provider.email_scope(&account());

    for _ in 0..2 {
        sync_mail_streamed(
            &provider,
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await
        .unwrap();
    }
    assert_survived(&store, &scope).await;
}

#[tokio::test]
async fn a_provider_supplied_thread_is_not_overridden_by_the_stored_one() {
    // The restore fills gaps; it never argues with a provider that knows. A JMAP
    // account whose server re-threads a message must see the new thread land, or the
    // stopgap becomes a way to pin threading to whatever was first stored.
    let moved = {
        let mut message = flag_only();
        message.thread = Some(ThreadRef::derived(
            ThreadId::try_from("thread-moved").unwrap(),
        ));
        message
    };
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![threaded("Quarterly report")],
    )
    .then_changing(vec![moved]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let scope = provider.email_scope(&account());

    for _ in 0..2 {
        sync_mail(
            &provider,
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
        )
        .await
        .unwrap();
    }
    let (message, indexed) = stored(&store, &scope).await;
    assert_eq!(
        message.thread.as_ref().map(|thread| &thread.id),
        Some(&ThreadId::try_from("thread-moved").unwrap())
    );
    assert_eq!(indexed, Some(ThreadId::try_from("thread-moved").unwrap()));
    assert_eq!(
        message.preview.as_deref(),
        Some("The numbers you asked for are attached."),
        "the preview it still did not send is still restored"
    );
}
