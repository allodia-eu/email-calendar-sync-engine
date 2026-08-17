//! Threading end to end: the sync itself groups unthreaded mail, a reply arriving later joins the
//! thread it belongs to, and provider-threaded mail is left alone (`threading.md`).
//!
//! The pass that used to do this after every sync is now [`Engine::rebuild_thread_index`], a
//! repair — so each case here asserts the grouping is right **without** calling it, and then that
//! calling it changes nothing. A rebuild that still had work to do is the shape of the bug this
//! phase removes.

use engine_api::{Engine, Message, ThreadId, ThreadRef};

use super::*;

/// The thread id of the message with the given provider key.
fn thread_of(messages: &[Message], key: &str) -> Option<ThreadId> {
    messages
        .iter()
        .find(|m| m.id.key().as_str() == key)
        .unwrap()
        .thread_id()
        .cloned()
}

#[tokio::test]
async fn a_sync_groups_unthreaded_mail_without_a_second_pass() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::threaded(), &account())
        .await
        .unwrap();

    // IMAP-shaped mail arrives carrying no thread id, and is threaded by the apply that stores
    // it: the reply (t2) with its original (t1), t3 on its own.
    let after = engine.messages(&account()).await.unwrap();
    assert!(thread_of(&after, "t1").is_some());
    assert_eq!(thread_of(&after, "t1"), thread_of(&after, "t2"));
    assert_ne!(thread_of(&after, "t1"), thread_of(&after, "t3"));
    assert_eq!(thread_of(&after, "t1").unwrap().as_str(), "a@h");

    // The rebuild agrees with what the sync already wrote, so it writes nothing. This is the
    // assertion that says the incremental answer *is* the derived one.
    let report = engine.rebuild_thread_index(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 0);
    assert_eq!(report.threads, 2);
}

#[tokio::test]
async fn a_reply_synced_later_joins_the_thread_it_answers() {
    // The reply lands in a second, cursored sync, long after its thread was first decided. It has
    // to find that thread through the stored message-id graph — nothing in its own page knows it.
    let provider = FakeProvider::threaded().adding_on_resync(vec![threaded_message(
        "t4",
        "a",
        "d@h",
        &["a@h"],
    )]);
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();

    let after = engine.messages(&account()).await.unwrap();
    assert_eq!(thread_of(&after, "t4"), thread_of(&after, "t1"));
    // The incumbent keeps its id: "a@h" is still the smallest owned Message-ID in the component,
    // so joining it costs no re-key and no list row moves.
    assert_eq!(thread_of(&after, "t4").unwrap().as_str(), "a@h");

    // The thread read returns all three members, so the message rows moved with it.
    let members = engine.mail_on_threads(&[account()], ["a@h"]).await.unwrap();
    assert_eq!(members.len(), 3);

    let report = engine.rebuild_thread_index(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 0, "the sync had already done it");
}

#[tokio::test]
async fn a_late_message_owning_a_smaller_id_rekeys_the_whole_thread() {
    // The thread id is the component's smallest owned Message-ID, so a late arrival owning a
    // smaller one re-keys the thread — every member, not just the newcomer. Hosts keying list
    // rows on thread_id see the change.
    let provider = FakeProvider::threaded().adding_on_resync(vec![threaded_message(
        "t4",
        "a",
        "0@h",
        &["a@h"],
    )]);
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();

    let after = engine.messages(&account()).await.unwrap();
    for key in ["t1", "t2", "t4"] {
        assert_eq!(
            thread_of(&after, key).unwrap().as_str(),
            "0@h",
            "{key} moved onto the newcomer's id"
        );
    }
    let rekeyed = engine.mail_on_threads(&[account()], ["0@h"]).await.unwrap();
    assert_eq!(rekeyed.len(), 3);
    let old = engine.mail_on_threads(&[account()], ["a@h"]).await.unwrap();
    assert!(old.is_empty(), "the old thread id no longer resolves");

    let report = engine.rebuild_thread_index(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 0, "the re-key already landed");
}

#[tokio::test]
async fn provider_threaded_mail_is_never_regrouped() {
    // A provider that assigns its own thread ids (JMAP/Gmail/Graph): neither the sync nor a
    // rebuild may touch them, even though t2's References would otherwise unite it with t1.
    let mut provider = FakeProvider::threaded();
    for (index, message) in provider.messages.iter_mut().enumerate() {
        message.thread = Some(ThreadRef::provider_assigned(
            ThreadId::try_from(format!("T{index}").as_str()).unwrap(),
        ));
    }
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();

    let after = engine.messages(&account()).await.unwrap();
    assert_eq!(after.len(), 3);
    assert!(
        after
            .iter()
            .all(|m| m.thread.as_ref().is_some_and(|t| !t.is_derived()))
    );
    assert_ne!(thread_of(&after, "t1"), thread_of(&after, "t2"));

    let report = engine.rebuild_thread_index(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 0);
}
