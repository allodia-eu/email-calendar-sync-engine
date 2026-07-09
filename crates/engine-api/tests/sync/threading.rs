//! Thread derivation end to end: grouping unthreaded mail, re-grouping it when a reply
//! arrives in a later sync, and leaving provider-threaded mail alone (`threading.md`).

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
async fn derives_and_persists_thread_ids_for_unthreaded_mail() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::threaded(), &account())
        .await
        .unwrap();

    // IMAP-shaped mail arrives without thread ids.
    let before = engine.messages(&account()).await.unwrap();
    assert!(before.iter().all(|m| m.thread_id().is_none()));

    // Derivation groups the reply (t2) with its original (t1); t3 stands alone.
    let report = engine.derive_mail_threads(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 3);
    assert_eq!(report.threads, 2);

    // The grouping is persisted: messages() now carries the derived thread id.
    let after = engine.messages(&account()).await.unwrap();
    assert!(thread_of(&after, "t1").is_some());
    assert_eq!(thread_of(&after, "t1"), thread_of(&after, "t2"));
    assert_ne!(thread_of(&after, "t1"), thread_of(&after, "t3"));

    // Re-deriving over unchanged mail rewrites nothing.
    let again = engine.derive_mail_threads(&account()).await.unwrap();
    assert_eq!(again.messages_assigned, 0);
    assert_eq!(again.threads, 2);
}

#[tokio::test]
async fn a_reply_synced_after_derivation_joins_its_existing_thread() {
    // Regression: derivation used to skip every message that already carried a thread id,
    // including the ones it had derived itself — so a reply arriving in a later sync could
    // only unite with mail from its own pass, and became a one-message thread.
    let provider = FakeProvider::threaded().adding_on_resync(vec![threaded_message(
        "t4",
        "a",
        "d@h",
        &["a@h"],
    )]);
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();
    engine.derive_mail_threads(&account()).await.unwrap();

    // The reply lands in a second, cursored sync — after t1's thread was derived.
    engine.sync_mail(&provider, &account()).await.unwrap();
    let report = engine.derive_mail_threads(&account()).await.unwrap();
    // Only the newcomer is written: the incumbent thread keeps its id, since "a@h" is
    // still the smallest owned Message-ID in the component.
    assert_eq!(report.messages_assigned, 1);
    assert_eq!(report.threads, 2);

    let after = engine.messages(&account()).await.unwrap();
    assert_eq!(thread_of(&after, "t4"), thread_of(&after, "t1"));
    assert_eq!(thread_of(&after, "t4").unwrap().as_str(), "a@h");

    // The mail index was re-projected too, so the thread read returns all three members.
    let members = engine.thread_messages(&account(), "a@h").await.unwrap();
    assert_eq!(members.len(), 3);
}

#[tokio::test]
async fn a_late_message_owning_a_smaller_id_rekeys_the_whole_thread() {
    // The thread id is the component's smallest owned Message-ID, so a late arrival owning
    // a smaller one re-keys the thread: every member is re-applied, payload and index row
    // alike. Hosts keying list rows on thread_id see the change.
    let provider = FakeProvider::threaded().adding_on_resync(vec![threaded_message(
        "t4",
        "a",
        "0@h",
        &["a@h"],
    )]);
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();
    engine.derive_mail_threads(&account()).await.unwrap();

    engine.sync_mail(&provider, &account()).await.unwrap();
    let report = engine.derive_mail_threads(&account()).await.unwrap();
    // t1 and t2 are re-keyed onto the newcomer's id, and t4 gains it.
    assert_eq!(report.messages_assigned, 3);

    let after = engine.messages(&account()).await.unwrap();
    for key in ["t1", "t2", "t4"] {
        assert_eq!(thread_of(&after, key).unwrap().as_str(), "0@h");
    }
    let rekeyed = engine.thread_messages(&account(), "0@h").await.unwrap();
    assert_eq!(rekeyed.len(), 3);
    let old = engine.thread_messages(&account(), "a@h").await.unwrap();
    assert!(old.is_empty(), "the old thread id no longer resolves");
}

#[tokio::test]
async fn derive_mail_threads_is_a_noop_for_provider_threaded_mail() {
    // A provider that assigns its own thread ids (JMAP/Gmail/Graph): derivation must not
    // touch them, even though t2's References would otherwise unite it with t1.
    let mut provider = FakeProvider::threaded();
    for (index, message) in provider.messages.iter_mut().enumerate() {
        message.thread = Some(ThreadRef::provider_assigned(
            ThreadId::try_from(format!("T{index}").as_str()).unwrap(),
        ));
    }
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();

    let report = engine.derive_mail_threads(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 0);

    // Every message keeps the provider's id, and the two remain separate threads.
    let after = engine.messages(&account()).await.unwrap();
    assert_eq!(after.len(), 3);
    assert!(
        after
            .iter()
            .all(|m| m.thread.as_ref().is_some_and(|t| !t.is_derived()))
    );
    assert_ne!(thread_of(&after, "t1"), thread_of(&after, "t2"));
}
