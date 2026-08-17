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

use std::collections::BTreeSet;

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
fn marked_seen() -> MailStateChange {
    MailStateChange::keywords(
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
    .then_changing_state(vec![marked_seen()]);
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
    .then_changing_state(vec![marked_seen()]);
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
    .then_changing_state(vec![marked_seen()]);
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
    crate::rebuild_thread_index(&store, &account(), worker(), Duration::from_mins(1))
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

    crate::rebuild_thread_index(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    intact_and_seen(&listed(&store).await, settled.as_str());
}

/// A rebuild over mail the sync already threaded assigns nothing — the first time and every time.
///
/// Two rules meet here. The sync threads what it applies, so by the time a rebuild runs there is
/// nothing left to decide. And the rebuild compares its computed assignment against the **stored
/// row**, not against the payload it rebuilt the graph from — comparing against the payload would
/// re-assign every message on every pass, forever, because a thread-only write leaves the payload
/// alone.
#[tokio::test]
async fn a_rebuild_over_mail_the_sync_threaded_writes_nothing() {
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

    let first = crate::rebuild_thread_index(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    assert_eq!(
        first.messages_assigned, 0,
        "the apply that stored the message already threaded it"
    );
    assert_eq!(first.threads, 1, "and it is on a conversation of its own");
    let second = crate::rebuild_thread_index(&store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap();
    assert_eq!(
        second.messages_assigned, 0,
        "nothing moved, so the pass converges instead of rewriting every message"
    );
}

/// The draft that lands in Drafts, addressed to someone worth suggesting later.
fn addressed_draft() -> Message {
    let mut draft = message("m1", "drafts", "Quarterly report");
    draft.envelope.to = vec![EmailAddress::named("Friend", "friend@example.test")];
    draft
}

/// The state change a send produces on a provider that files in place: the same object, now in
/// Sent. JMAP's `EmailSubmission/set` moves the draft with `onSuccessUpdateEmail` and Gmail's
/// `drafts.send` adds `SENT` to the message the draft already had — neither mints a new id, so
/// this never arrives as a whole object.
fn moved_to_sent() -> MailStateChange {
    MailStateChange::new(
        key("m1"),
        engine_core::mail::MailState::with_keywords(BTreeSet::new())
            .filed_in(Memberships::of_one(MailboxId::try_from("sent").unwrap())),
    )
}

/// The two mailboxes such a send moves between.
fn drafts_and_sent() -> Vec<Mailbox> {
    vec![
        mailbox("drafts", "Drafts", Some(MailboxRole::Drafts)),
        mailbox("sent", "Sent", Some(MailboxRole::Sent)),
    ]
}

/// A message that **enters Sent through a state change** still yields its recipients.
///
/// This is how a message usually becomes sent, not an edge case: the draft is already stored, so
/// the send reaches the next sync as an `Email/changes` `updated` id or a `labelsAdded` record
/// for a key we hold — never as a `created`. Reading only the whole objects an update carries
/// means the address you just wrote to never enters autosuggest.
///
/// The change carries filing and keywords, no envelope, so the recipients come from the stored
/// payload.
#[tokio::test]
async fn a_state_change_into_sent_observes_the_recipients_whole_scope() {
    let provider = FakeMail::new(drafts_and_sent(), vec![addressed_draft()])
        .then_changing_state(vec![moved_to_sent()]);
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
    assert!(
        store.recipient_interactions(None).await.unwrap().is_empty(),
        "a draft is not a sent message, so nothing is observed from it yet"
    );

    // Second pass: the cursor exists, so the fake emits the move into Sent.
    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    let interactions = store.recipient_interactions(None).await.unwrap();
    assert_eq!(
        interactions
            .iter()
            .map(|item| item.email.as_str())
            .collect::<Vec<_>>(),
        vec!["friend@example.test"],
        "the send is observed from the state change that filed it in Sent"
    );
    assert_eq!(interactions[0].sent_count, 1);
    assert_eq!(interactions[0].name.as_deref(), Some("Friend"));
}

/// Same claim against the streaming driver, which builds its batch in its own loop.
#[tokio::test]
async fn a_state_change_into_sent_observes_the_recipients_streamed() {
    let provider = FakeMail::new(drafts_and_sent(), vec![addressed_draft()])
        .then_changing_state(vec![moved_to_sent()]);
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
    assert_eq!(
        store
            .recipient_interactions(None)
            .await
            .unwrap()
            .iter()
            .map(|item| item.email.as_str())
            .collect::<Vec<_>>(),
        vec!["friend@example.test"]
    );
}

/// A mark-read on a message sitting in Sent does not observe it a second time.
///
/// The observation is keyed by `(account, source message, email)`, so a replay is idempotent —
/// but a state change that never reaches Sent must not even look, which is what keeps an
/// ordinary mark-read free of a payload read.
#[tokio::test]
async fn a_state_change_that_stays_out_of_sent_observes_nothing() {
    let provider = FakeMail::new(drafts_and_sent(), vec![addressed_draft()])
        .then_changing_state(vec![marked_seen()]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

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
    assert!(
        store.recipient_interactions(None).await.unwrap().is_empty(),
        "the message is still in Drafts, so a keyword change says nothing about recipients"
    );
}

/// A state change that files the message somewhere that is **not** Sent observes nothing.
///
/// The change carries filing here, unlike the mark-read above, so this is the branch that has
/// to compare it against the account's Sent collections rather than skip on absence. An archive
/// is the same kind of event as a send on JMAP and Gmail — one object, one new mailbox set — and
/// reading every filing change as a send would put every correspondent you have ever archived
/// into autosuggest as someone you wrote to.
#[tokio::test]
async fn a_state_change_that_files_the_message_elsewhere_observes_nothing() {
    let mut mailboxes = drafts_and_sent();
    mailboxes.push(mailbox("archive", "Archive", Some(MailboxRole::Archive)));
    let archived = MailStateChange::new(
        key("m1"),
        engine_core::mail::MailState::with_keywords(BTreeSet::new())
            .filed_in(Memberships::of_one(MailboxId::try_from("archive").unwrap())),
    );
    let provider =
        FakeMail::new(mailboxes, vec![addressed_draft()]).then_changing_state(vec![archived]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

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
    assert!(
        store.recipient_interactions(None).await.unwrap().is_empty(),
        "filing a message in Archive is not evidence that anyone was written to"
    );
}

/// A state change filing an **unsynced** message in Sent observes nothing, and is not an error.
///
/// Gmail's history and JMAP's `Email/changes` are account-global while the sync is windowed, so
/// a change for mail older than the window is ordinary traffic. There is no stored payload to
/// read recipients from, and inventing an observation from a key alone would mean a suggestion
/// with no message behind it. Skipping matches the one-time backfill, which drops a payload
/// whose row is gone for the same reason.
#[tokio::test]
async fn a_state_change_into_sent_for_an_unsynced_message_observes_nothing() {
    let out_of_window = MailStateChange::new(
        key("never-synced"),
        engine_core::mail::MailState::with_keywords(BTreeSet::new())
            .filed_in(Memberships::of_one(MailboxId::try_from("sent").unwrap())),
    );
    let provider = FakeMail::new(drafts_and_sent(), vec![addressed_draft()])
        .then_changing_state(vec![out_of_window]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    for _ in 0..2 {
        sync_mail(
            &provider,
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
        )
        .await
        .expect("a change for an unknown key is ordinary traffic, not a failure");
    }
    assert!(
        store.recipient_interactions(None).await.unwrap().is_empty(),
        "there is no stored message to read recipients from, so nothing is claimed"
    );
}
