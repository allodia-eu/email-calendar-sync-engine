//! The read and search surface over synced data: windowed and thread reads, full-text
//! and structured mail/calendar search, malformed-query rejection, and the mailbox/
//! message and calendar/event lists.

use engine_api::{ApiError, Engine, TimeZoneId};

use super::*;

#[tokio::test]
async fn windowed_read_returns_the_newest_and_thread_read_is_age_independent() {
    // Inbox: a very old message `old` and a much newer reply `new` on the SAME derived thread,
    // plus three unrelated newer messages — so the newest-N window drops `old` while the thread
    // still holds it, and `old` is only reachable by thread/key, never by the window.
    let provider = FakeProvider {
        messages: vec![
            dated_message("old", "old@h", &[], "2020-01-01T00:00:00Z"),
            dated_message("f1", "f1@h", &[], "2026-02-01T00:00:00Z"),
            dated_message("f2", "f2@h", &[], "2026-03-01T00:00:00Z"),
            dated_message("f3", "f3@h", &[], "2026-04-01T00:00:00Z"),
            // Replies to `old` (shared reference) → the engine derives one thread for both.
            dated_message("new", "new@h", &["old@h"], "2026-05-01T00:00:00Z"),
        ],
        ..FakeProvider::new()
    };
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();
    // IMAP-shaped mail arrives unthreaded; derivation groups `new` with `old` (shared reference).
    engine.derive_mail_threads(&account()).await.unwrap();

    // A window of 2 keeps only the two newest by date, newest first — `old` is outside it.
    let windowed = engine.mail_window(&[account()], 2).await.unwrap();
    let keys: Vec<&str> = windowed.iter().map(|m| m.mail.key.as_str()).collect();
    assert_eq!(keys, vec!["new", "f3"], "newest two, newest first");
    // The row carries what the list renders, so no payload is opened to draw it.
    assert_eq!(windowed[0].mail.subject.as_deref(), Some("subject"));
    assert!(windowed[0].mail.flags.is_unread());
    assert!(
        !windowed[0].mailboxes.is_empty(),
        "and which folder it is in"
    );

    // `old` and `new` share the derived thread; reading it returns BOTH, even though `old` is
    // far outside the window — the whole point of the design.
    let thread = engine
        .messages(&account())
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id.key().as_str() == "new")
        .and_then(|m| m.thread_id().cloned())
        .expect("the reply is threaded");
    let mut members: Vec<String> = engine
        .mail_on_threads(&[account()], [thread.as_str()])
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.mail.key.as_str().to_owned())
        .collect();
    members.sort();
    assert_eq!(members, vec!["new".to_owned(), "old".to_owned()]);

    // Batched completion: over the window (`new`, `f3`), one call pulls every shown
    // conversation's members. Unrelated threads (f1/f2/f3) aren't asked for, so they don't come
    // back, and the host drops the keys it already holds.
    let completed: Vec<String> = engine
        .mail_on_threads(&[account()], [thread.as_str()])
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.mail.key.as_str().to_owned())
        .filter(|key| key != "new" && key != "f3")
        .collect();
    assert_eq!(
        completed,
        vec!["old".to_owned()],
        "only the out-of-window member of the shown conversation"
    );

    // A specific out-of-window key still resolves directly (open/reply/search-hit resolution).
    let resolved = engine
        .mail_by_keys(&account(), &[ProviderKey::new("old").unwrap()])
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].mail.key.as_str(), "old");
    // …and so does the whole normalized object, which is what opening a message reads.
    let opened = engine
        .messages_by_keys(&account(), &[ProviderKey::new("old").unwrap()])
        .await
        .unwrap();
    assert_eq!(opened[0].id.key().as_str(), "old");
}

#[tokio::test]
async fn searches_synced_mail() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();

    // Full-text over the indexed subject: "report" matches m1's "Quarterly report".
    let m1 = message("m1", "a", "Quarterly report").id.key().clone();
    let m2 = message("m2", "a", "Lunch plans").id.key().clone();
    let report = engine.search_mail(&account(), "report", 10).await.unwrap();
    assert_eq!(report.keys(), vec![m1.clone()]);
    assert!(report.coverage.is_complete());

    // A structured membership filter: both messages live in mailbox "a".
    let in_a = engine
        .search_mail(&account(), "mailbox:a", 10)
        .await
        .unwrap();
    let keys = in_a.keys();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&m1) && keys.contains(&m2));
}

#[tokio::test]
async fn searches_synced_calendar() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();

    // The event is a member of calendar "work"; the calendar-domain scopes are
    // enumerated and searched, not hard-coded.
    let evt = event("evt-1", "uid-1@h", "work").id.key().clone();
    let in_work = engine
        .search_calendar(&account(), "calendar:work", 10)
        .await
        .unwrap();
    assert_eq!(in_work.keys(), vec![evt]);
    assert!(in_work.coverage.is_complete());
}

#[tokio::test]
async fn search_rejects_a_malformed_query() {
    let engine = Engine::open_in_memory().unwrap();
    let mail_err = engine
        .search_mail(&account(), "from:", 10)
        .await
        .unwrap_err();
    assert!(matches!(mail_err, ApiError::Query(_)), "got {mail_err:?}");
    let cal_err = engine
        .search_calendar(&account(), "after:nope", 10)
        .await
        .unwrap_err();
    assert!(matches!(cal_err, ApiError::Query(_)), "got {cal_err:?}");
}

#[tokio::test]
async fn search_on_an_unsynced_account_is_empty() {
    let engine = Engine::open_in_memory().unwrap();
    // No sync has run, so the account has no scopes: an empty, vacuously complete answer.
    let results = engine.search_mail(&account(), "report", 10).await.unwrap();
    assert!(results.hits.is_empty());
    assert!(results.coverage.is_complete());
}

#[tokio::test]
async fn lists_synced_mailboxes_and_messages() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();

    // The two synced mailboxes, carrying their real names (not just keys).
    let mailboxes = engine.mailboxes(&account()).await.unwrap();
    let names: Vec<&str> = mailboxes.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(mailboxes.len(), 2);
    assert!(names.contains(&"Inbox") && names.contains(&"Archive"));

    // The two synced messages, carrying their real envelope subjects.
    let messages = engine.messages(&account()).await.unwrap();
    let subjects: Vec<&str> = messages
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    assert_eq!(messages.len(), 2);
    assert!(subjects.contains(&"Quarterly report") && subjects.contains(&"Lunch plans"));

    // An account that never synced has neither.
    let other = AccountId::try_from("nobody").unwrap();
    assert!(engine.mailboxes(&other).await.unwrap().is_empty());
    assert!(engine.messages(&other).await.unwrap().is_empty());
}

#[tokio::test]
async fn lists_synced_calendars_and_events() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();

    // The one synced calendar, carrying its real name (not just a key).
    let calendars = engine.calendars(&account()).await.unwrap();
    assert_eq!(calendars.len(), 1);
    assert_eq!(calendars[0].name, "Work");

    // The one synced event, carrying its real cross-system uid through the store.
    let events = engine.events(&account()).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, Uid::new("uid-1@h").unwrap());

    // An account that never synced has neither.
    let other = AccountId::try_from("nobody").unwrap();
    assert!(engine.calendars(&other).await.unwrap().is_empty());
    assert!(engine.events(&other).await.unwrap().is_empty());
}

/// `events_by_keys` resolves a *named handful* of events without reading the rest — the
/// targeted read the event-detail view and the occurrence→master join use so a large
/// calendar's whole event history is never deserialized to answer for one block.
#[tokio::test]
async fn events_by_keys_resolves_only_the_named_events() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let provider = FakeProvider {
        events: vec![
            event("evt-a", "uid-a@h", "work"),
            event("evt-b", "uid-b@h", "work"),
            event("evt-c", "uid-c@h", "work"),
        ],
        ..FakeProvider::new()
    };
    engine
        .sync_calendar(&provider, &account(), horizon(), &zone)
        .await
        .unwrap();

    // Two of the three, by key: exactly those two come back, and nothing else.
    let mut got: Vec<String> = engine
        .events_by_keys(
            &account(),
            &[
                ProviderKey::new("evt-a").unwrap(),
                ProviderKey::new("evt-c").unwrap(),
            ],
        )
        .await
        .unwrap()
        .iter()
        .map(|e| e.id.key().as_str().to_owned())
        .collect();
    got.sort();
    assert_eq!(got, vec!["evt-a".to_owned(), "evt-c".to_owned()]);

    // A key that isn't there is simply absent (not an error), and an empty request
    // touches nothing — the join passes an empty set when a window holds no occurrences.
    let mixed = engine
        .events_by_keys(
            &account(),
            &[
                ProviderKey::new("evt-b").unwrap(),
                ProviderKey::new("evt-missing").unwrap(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(mixed.len(), 1);
    assert_eq!(mixed[0].id.key().as_str(), "evt-b");
    assert!(
        engine
            .events_by_keys(&account(), &[])
            .await
            .unwrap()
            .is_empty()
    );

    // An account that never synced resolves nothing rather than erroring.
    let other = AccountId::try_from("nobody").unwrap();
    assert!(
        engine
            .events_by_keys(&other, &[ProviderKey::new("evt-a").unwrap()])
            .await
            .unwrap()
            .is_empty()
    );
}
