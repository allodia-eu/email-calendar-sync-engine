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
    let windowed = engine.messages_windowed(&account(), 2).await.unwrap();
    let keys: Vec<&str> = windowed.iter().map(|m| m.id.key().as_str()).collect();
    assert_eq!(keys, vec!["new", "f3"], "newest two, newest first");

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
        .thread_messages(&account(), thread.as_str())
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id.key().as_str().to_owned())
        .collect();
    members.sort();
    assert_eq!(members, vec!["new".to_owned(), "old".to_owned()]);

    // Batched completion: over the window (`new`, `f3`), `thread_members` pulls the thread's
    // out-of-window member `old` in ONE pass, and `exclude` keeps `new` (already in the window)
    // from being re-read. Unrelated threads (f1/f2/f3) aren't asked for, so they don't come back.
    let window_threads: std::collections::HashSet<String> = [thread.as_str().to_owned()].into();
    let window_keys: std::collections::HashSet<String> = ["new".to_owned(), "f3".to_owned()].into();
    let extra: Vec<String> = engine
        .thread_members(&account(), &window_threads, &window_keys)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id.key().as_str().to_owned())
        .collect();
    assert_eq!(
        extra,
        vec!["old".to_owned()],
        "only the out-of-window member, excluding `new`"
    );

    // A specific out-of-window key still resolves directly (open/reply/search-hit resolution).
    let resolved = engine
        .messages_by_keys(&account(), &[ProviderKey::new("old").unwrap()])
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].id.key().as_str(), "old");
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

/// The read a calendar grid pages over: `occurrences_in` materializes a recurring
/// series into one row per instance in the window, where `events()` — which returns
/// the projected *envelope* — reports the whole series exactly once, at its start.
///
/// This is the difference between a week grid that shows Monday's standup and one
/// that shows it only in the week the series began. It also pins the window's
/// boundary: an instance landing exactly on the exclusive upper bound belongs to the
/// next page, so paging a grid forward never renders the same meeting twice.
#[tokio::test]
async fn occurrences_expand_a_recurrence_that_the_event_list_reports_once() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    // A standup every Monday at 09:00 UTC for four weeks, from Mon 2026-07-06.
    let provider = FakeProvider {
        events: vec![weekly_event(
            "evt-standup",
            "uid-standup@h",
            LocalDateTime::new(2026, 7, 6, 9, 0, 0).unwrap(),
            4,
        )],
        ..FakeProvider::new()
    };
    engine
        .sync_calendar(&provider, &account(), horizon(), &zone)
        .await
        .unwrap();

    // The event list sees the series as ONE object — the master envelope. A grid
    // laid out from this renders the standup in one week and no other.
    let events = engine.events(&account()).await.unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].recurrence.is_some());

    // The occurrence read sees all four instances.
    let all = Horizon::new(
        "2026-07-01T00:00:00Z".parse().unwrap(),
        "2026-08-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let starts: Vec<String> = engine
        .occurrences_in(&account(), all)
        .await
        .unwrap()
        .iter()
        .map(|row| row.start.to_string())
        .collect();
    assert_eq!(
        starts,
        vec![
            "2026-07-06T09:00:00Z",
            "2026-07-13T09:00:00Z",
            "2026-07-20T09:00:00Z",
            "2026-07-27T09:00:00Z",
        ]
    );

    // One week of it: exactly the instance in that week. The next Monday's instance
    // sits on the window's exclusive upper bound, so it is the next page's, not this
    // one's — page forward and it appears exactly once, never twice.
    let week = Horizon::new(
        "2026-07-06T00:00:00Z".parse().unwrap(),
        "2026-07-13T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let this_week = engine.occurrences_in(&account(), week).await.unwrap();
    assert_eq!(this_week.len(), 1);
    assert_eq!(this_week[0].start.to_string(), "2026-07-06T09:00:00Z");
    // Every row points back at the master, so a host joins it to `events()` for the
    // title, calendar membership, and participants.
    assert_eq!(this_week[0].event, events[0].id.key().clone());

    // A week the series does not reach has none — and an account that never synced
    // has none, rather than erroring.
    let before = Horizon::new(
        "2026-06-01T00:00:00Z".parse().unwrap(),
        "2026-06-08T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    assert!(
        engine
            .occurrences_in(&account(), before)
            .await
            .unwrap()
            .is_empty()
    );
    let other = AccountId::try_from("nobody").unwrap();
    assert!(
        engine
            .occurrences_in(&other, week)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Widening the horizon and re-syncing does **not** backfill occurrences —
/// `expand_horizon` is the only thing that does.
///
/// A sync expands only the objects its delta *changed*. Once an account is synced, a
/// provider reporting "nothing changed" (the normal steady state) derives no
/// occurrences at all — so a host that pages its grid past what the first sync
/// materialized, then re-syncs to fetch it, gets an **empty window, permanently**. This
/// pins that the escape hatch works, and that re-syncing alone is not one.
#[tokio::test]
async fn a_widened_horizon_needs_an_expansion_because_a_resync_will_not_backfill_it() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let provider = FakeProvider {
        events: vec![weekly_event(
            "evt-standup",
            "uid-standup@h",
            LocalDateTime::new(2026, 7, 6, 9, 0, 0).unwrap(),
            12,
        )],
        ..FakeProvider::new()
    };

    // Sync with a horizon covering only July: the August instances are never materialized.
    let july = Horizon::new(
        "2026-07-01T00:00:00Z".parse().unwrap(),
        "2026-08-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    engine
        .sync_calendar(&provider, &account(), july, &zone)
        .await
        .unwrap();

    let august = Horizon::new(
        "2026-08-01T00:00:00Z".parse().unwrap(),
        "2026-09-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    assert!(
        engine
            .occurrences_in(&account(), august)
            .await
            .unwrap()
            .is_empty(),
        "August was never expanded, so it reads empty"
    );

    // Re-sync with the WIDER horizon. The provider reports no changes (it has a cursor
    // now), so nothing is re-derived — August is *still* empty. This is the trap: the
    // grid looks like an empty month, and syncing again never fixes it.
    let rest_of_year = Horizon::new(
        "2026-07-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    engine
        .sync_calendar(&provider, &account(), rest_of_year, &zone)
        .await
        .unwrap();
    assert!(
        engine
            .occurrences_in(&account(), august)
            .await
            .unwrap()
            .is_empty(),
        "a re-sync derives only what CHANGED, so a widened horizon backfills nothing"
    );

    // Expanding the horizon re-derives every stored event, with no network. Now August
    // has its instances.
    let report = engine
        .expand_horizon(&account(), rest_of_year, &zone)
        .await
        .unwrap();
    assert_eq!(report.occurrences, 12);
    assert!(report.unexpandable.is_empty());

    let starts: Vec<String> = engine
        .occurrences_in(&account(), august)
        .await
        .unwrap()
        .iter()
        .map(|row| row.start.to_string())
        .collect();
    assert_eq!(
        starts,
        vec![
            "2026-08-03T09:00:00Z",
            "2026-08-10T09:00:00Z",
            "2026-08-17T09:00:00Z",
            "2026-08-24T09:00:00Z",
            "2026-08-31T09:00:00Z",
        ]
    );

    // Re-expanding is idempotent: rows replace, they do not accumulate.
    let again = engine
        .expand_horizon(&account(), rest_of_year, &zone)
        .await
        .unwrap();
    assert_eq!(again.occurrences, 12);
    assert_eq!(
        engine
            .occurrences_in(&account(), august)
            .await
            .unwrap()
            .len(),
        5
    );
}

/// An event whose recurrence the expander cannot handle materializes **zero**
/// occurrences — so it is stored, and invisible to every range read. Both the sync and
/// the expansion report it by name, rather than dropping it in silence.
///
/// Without this, an event that a user can see in a flat agenda simply *vanishes* from a
/// grid, with nothing anywhere saying why.
#[tokio::test]
async fn an_unexpandable_recurrence_is_reported_rather_than_silently_dropped() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    // `BYSETPOS` ("the last working day of the month") is outside the expander's
    // supported subset — a rule real servers do emit.
    let mut event = weekly_event(
        "evt-payday",
        "uid-payday@h",
        LocalDateTime::new(2026, 7, 6, 9, 0, 0).unwrap(),
        12,
    );
    let mut rule = RecurrenceRule::new(Frequency::Monthly);
    rule.by_set_position = vec![-1];
    event.recurrence = Some(Recurrence::from_rule(rule));
    let provider = FakeProvider {
        events: vec![event],
        ..FakeProvider::new()
    };

    let year = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let report = engine
        .sync_calendar(&provider, &account(), year, &zone)
        .await
        .unwrap();

    // The sync succeeds and stores the event — one unsupported rule must never fail a
    // whole calendar's sync.
    assert_eq!(engine.events(&account()).await.unwrap().len(), 1);
    // ...but it expands to nothing, so a grid over the whole year shows it nowhere.
    assert!(
        engine
            .occurrences_in(&account(), year)
            .await
            .unwrap()
            .is_empty()
    );
    // ...and THAT is reported, by key and reason, rather than silently swallowed.
    assert_eq!(report.unexpandable.len(), 1);
    assert_eq!(report.unexpandable[0].event.as_str(), "evt-payday");
    assert!(
        report.unexpandable[0].reason.contains("unsupported"),
        "got {:?}",
        report.unexpandable[0].reason
    );

    // The expansion path reports it too, so a host that only ever advances the horizon
    // still learns the event cannot be shown.
    let expanded = engine
        .expand_horizon(&account(), year, &zone)
        .await
        .unwrap();
    assert_eq!(expanded.occurrences, 0);
    assert_eq!(expanded.unexpandable.len(), 1);
    assert_eq!(expanded.unexpandable[0].event.as_str(), "evt-payday");
}
