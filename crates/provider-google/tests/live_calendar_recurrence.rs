//! Gated live checks for **recurring** calendar writes over Google Calendar: creating a
//! series, changing and removing its rule, and removing one occurrence of it.
//!
//! Its own file rather than an addition to `live_calendar.rs`, which is close to the
//! 500-line cap; the shared setup lives in `common`.

mod common;

use common::*;
use engine_core::{
    calendar::Event,
    ids::{CalendarId, Uid},
    membership::Memberships,
    sync::SyncUpdate,
    time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDeletion, EventDraft, Provider};

/// A zoned wall clock in the zone the probes author in.
fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// Creating a **recurring** event, and reading the rule back off the server.
///
/// The offline suite proves only the body we build — the fake answers canned bytes
/// whatever it is sent (`AGENTS.md`). Only a real create says whether Google accepts the
/// `RRULE` line this adapter renders, and only a real read-back says whether the rule that
/// came home is the rule that went out.
#[tokio::test]
async fn live_calendar_creates_a_recurring_event() {
    use engine_core::{
        calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
        ids::Uid,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    };
    use engine_provider::{DraftRecurrence, EventDeletion, EventDraft};

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_creates_a_recurring_event: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-08-23T10:00:00Z".parse().unwrap();
    let zoned = |s: &str| CalendarDateTime::Zoned {
        local: s.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };

    // Every Monday until 26 October — the "ends on this day" shape the product's repeat
    // picker produces, and the one that obliges `UNTIL` in UTC (RFC 5545 §3.3.10). The
    // resolved instant is stated because no adapter carries tzdata: 23:59:59 in
    // Europe/Amsterdam is 22:59:59Z, CEST being UTC+2 on that date.
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    rule.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());

    let draft = EventDraft::new(
        cal.clone(),
        Uid::new(format!("live-recur-{}@example.test", std::process::id())).unwrap(),
        "Live recurrence probe",
        zoned("2026-09-07T09:30:00"),
        zoned("2026-09-07T10:00:00"),
        stamp,
    )
    .repeating(DraftRecurrence::ending_at(
        rule.clone(),
        "2026-10-26T22:59:59Z".parse().unwrap(),
    ));

    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create a recurring event");

    // Read it back through the adapter's own sync path: what a host would see.
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    let stored = objects
        .iter()
        .find(|e| e.id == created.event)
        .expect("the created series is in the snapshot");

    assert!(
        stored.is_recurring(),
        "the created event came back as a series master"
    );
    let stored_rule = &stored.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(stored_rule.frequency, rule.frequency);
    assert_eq!(stored_rule.by_day, rule.by_day);
    // Google echoes the UNTIL back as the UTC instant it was sent, which the shared parser
    // reads as that instant's own wall clock — so the round trip lands on 22:59:59, not on
    // the 23:59:59 Amsterdam clock it was authored from. Pinning it keeps the asymmetry
    // visible: a host that wants the authored clock re-resolves through the event's zone.
    assert_eq!(
        stored_rule.bound,
        RecurrenceBound::Until("2026-10-26T22:59:59".parse().unwrap()),
    );

    let mut base = engine_core::calendar::Event::new(
        created.event.clone(),
        created.uid.clone(),
        engine_core::membership::Memberships::of_one(cal),
        zoned("2026-09-07T09:30:00"),
    );
    base.revisions = created.revisions.clone();
    // Unguarded: cancelling an occurrence moved the series' own ETag, and this cleanup is
    // not the thing under test.
    provider
        .delete_event(
            &account(),
            None,
            &EventDeletion::unconditional(created.event.clone(), created.uid.clone()),
        )
        .await
        .expect("delete the probe series");
}

/// Changing and removing a rule on Google.
///
/// The clear is the half worth a live test on its own: an adapter can only *claim* it
/// removed a rule, and a patch that quietly changed nothing would look identical from
/// here. Google accepts both an empty array and `null` for this — both were measured, and
/// this test was watched to fail on neither, which is why the assertion is on the
/// **read-back** rather than on the request we sent.
#[tokio::test]
async fn live_calendar_changes_and_removes_a_rule() {
    use engine_core::{
        calendar::{Event, Frequency, NDay, RecurrenceRule, Weekday},
        ids::Uid,
        membership::Memberships,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    };
    use engine_provider::{
        DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget,
    };

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_changes_and_removes_a_rule: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-08-23T10:00:00Z".parse().unwrap();
    let zoned = |s: &str| CalendarDateTime::Zoned {
        local: s.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };
    let weekly_on = |day: Weekday| {
        let mut rule = RecurrenceRule::new(Frequency::Weekly);
        rule.by_day = vec![NDay {
            day,
            nth_of_period: None,
        }];
        rule
    };

    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                cal.clone(),
                Uid::new(format!("live-rule-{}@example.test", std::process::id())).unwrap(),
                "Live rule-edit probe",
                zoned("2026-09-07T09:30:00"),
                zoned("2026-09-07T10:00:00"),
                stamp,
            )
            .repeating(DraftRecurrence::new(weekly_on(Weekday::Mo))),
        )
        .await
        .expect("create a recurring event");

    let read_back = |id: engine_core::ids::EventId| {
        let provider = &provider;
        async move {
            let events = provider
                .sync_events(&account(), None)
                .await
                .expect("sync events");
            let SyncUpdate::Snapshot { objects, .. } = events.update else {
                panic!("expected an event snapshot");
            };
            objects
                .into_iter()
                .find(|e| e.id == id)
                .expect("the series is in the snapshot")
        }
    };

    let mut base = Event::new(
        created.event.clone(),
        created.uid.clone(),
        Memberships::of_one(cal.clone()),
        zoned("2026-09-07T09:30:00"),
    );
    base.revisions = created.revisions.clone();
    let changed = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp).recurrence(DraftRecurrence::new(weekly_on(Weekday::We))),
            ),
        )
        .await
        .expect("change the rule");
    assert_eq!(
        read_back(created.event.clone())
            .await
            .recurrence
            .unwrap()
            .rules[0]
            .by_day,
        weekly_on(Weekday::We).by_day,
        "Google stored the new rule"
    );

    base.revisions = changed.revisions.clone();
    let cleared = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp).clear_recurrence(),
            ),
        )
        .await
        .expect("remove the rule");
    assert!(
        !read_back(created.event.clone()).await.is_recurring(),
        "the rule is gone from the server's own copy, not just from the request"
    );

    base.revisions = cleared.revisions.clone();
    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe event");
}

/// Removing **one occurrence** of a series, at the id Google derives from its original
/// start in UTC.
///
/// The read-back is the whole test, and not a formality: a `DELETE` of an id that names no
/// occurrence answers `404`, and this verb reads `404` as "already gone" — correctly, for an
/// idempotent delete. So an id built from the wall clock as if it were UTC would report a
/// delete that never happened, and only the server's own account of what changed can tell
/// the two apart. Here that account is the delta's **removal set**: Google reports a
/// cancelled instance as a `status: "cancelled"` entry, which the reader passes on as a
/// removal under the instance's own id.
#[tokio::test]
async fn live_calendar_removes_one_occurrence() {
    use core::num::NonZeroU32;

    use engine_core::calendar::{Frequency, RecurrenceBound, RecurrenceRule};
    use engine_provider::{DraftRecurrence, Occurrence};

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_removes_one_occurrence: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-08-23T10:00:00Z".parse().unwrap();

    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());

    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                cal.clone(),
                Uid::new(format!("live-occ-{}@example.test", std::process::id())).unwrap(),
                "Live occurrence-delete probe",
                zoned("2026-09-07T09:30:00"),
                zoned("2026-09-07T10:00:00"),
                stamp,
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    // The cursor is taken *after* the create, so the delta below carries the cancellation.
    let cursor = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events")
        .next_cursor;

    let mut base = Event::new(
        created.event.clone(),
        created.uid.clone(),
        Memberships::of_one(cal.clone()),
        zoned("2026-09-07T09:30:00"),
    );
    base.revisions = created.revisions.clone();

    // 09:30 Amsterdam on 14 September is 07:30Z — CEST, and the resolution the caller owns.
    provider
        .delete_event(
            &account(),
            Some(&base),
            &EventDeletion::occurrence(
                &base,
                Occurrence::at(
                    zoned("2026-09-14T09:30:00"),
                    "2026-09-14T07:30:00Z".parse().unwrap(),
                ),
                stamp,
            ),
        )
        .await
        .expect("remove one occurrence");

    let delta = provider
        .sync_events(&account(), Some(&cursor))
        .await
        .expect("sync the change");
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &delta.update
    else {
        panic!("expected a delta");
    };
    let expected = format!("{}_20260914T073000Z", created.event.key().as_str());
    assert!(
        removed.iter().any(|key| key.as_str() == expected),
        "the server reports that occurrence as gone: {removed:?}"
    );
    assert!(
        !removed.iter().any(|key| key == created.event.key()),
        "and the series itself is not among them: {removed:?}"
    );
    let _ = changed;

    // Unguarded: cancelling an occurrence moved the series' own ETag, and this cleanup is
    // not the thing under test.
    provider
        .delete_event(
            &account(),
            None,
            &EventDeletion::unconditional(created.event.clone(), created.uid.clone()),
        )
        .await
        .expect("delete the probe series");
}
