//! Calendar occurrence materialization over a horizon: a recurrence the event list reports
//! once but `occurrences_in` expands per instance, the expansion escape-hatch a re-sync will
//! not substitute for (and its idempotent skip over an unchanged window), and an unexpandable
//! rule reported rather than silently dropped. Split from `reads.rs` to keep each test file
//! under the size limit; a sibling `sync/` submodule sharing its providers and fixtures.

use engine_api::{Engine, TimeZoneId};

use super::*;

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

    // Re-expanding over the SAME window is a no-op, and now says so: the scope's stored
    // ExpansionWindow already matches, so the pass skips it rather than re-deriving every
    // event under a held lease — the optimization that keeps a routine refresh from blocking
    // a concurrent read for seconds. The rows are untouched (idempotent either way).
    let again = engine
        .expand_horizon(&account(), rest_of_year, &zone)
        .await
        .unwrap();
    assert_eq!(
        again.occurrences, 0,
        "nothing re-derived over an unchanged window"
    );
    assert_eq!(
        again.skipped, 1,
        "the one event scope was skipped, lease untaken"
    );
    assert_eq!(
        engine
            .occurrences_in(&account(), august)
            .await
            .unwrap()
            .len(),
        5,
        "the occurrences the earlier expand wrote are still there, unchanged"
    );

    // A DIFFERENT window is not skipped — a horizon advance still re-expands, so paging the
    // grid forward keeps materializing. (The window carries the zone too, so a zone change
    // re-expands the same way; only a bare tzdata bump with an identical window is not caught.)
    let into_next_year = Horizon::new(
        "2026-07-01T00:00:00Z".parse().unwrap(),
        "2027-02-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let widened = engine
        .expand_horizon(&account(), into_next_year, &zone)
        .await
        .unwrap();
    assert_eq!(
        widened.skipped, 0,
        "a moved window is re-expanded, not skipped"
    );
    assert!(
        widened.occurrences >= 12,
        "the wider horizon re-derived the series"
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
    assert_eq!(report.events.unexpandable.len(), 1);
    assert_eq!(report.events.unexpandable[0].event.as_str(), "evt-payday");
    assert!(
        report.events.unexpandable[0].reason.contains("unsupported"),
        "got {:?}",
        report.events.unexpandable[0].reason
    );

    // The expansion path reports it too, so a host that only ever advances the horizon
    // still learns the event cannot be shown. Expand over a WIDER window than the sync
    // covered, so the pass actually runs (an identical window would be skipped as already
    // expanded) — a horizon advance is exactly when a host reaches for this.
    let wider = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-06-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let expanded = engine
        .expand_horizon(&account(), wider, &zone)
        .await
        .unwrap();
    assert_eq!(expanded.occurrences, 0);
    assert_eq!(
        expanded.skipped, 0,
        "a wider window runs, it is not skipped"
    );
    assert_eq!(expanded.unexpandable.len(), 1);
    assert_eq!(expanded.unexpandable[0].event.as_str(), "evt-payday");
}
