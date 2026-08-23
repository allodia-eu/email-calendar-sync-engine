//! Gated live checks for **recurring** calendar writes over Microsoft Graph: creating a
//! series, changing and removing its rule, and removing one occurrence of it.
//!
//! Its own file rather than an addition to `live_calendar.rs`, which is close to the
//! 500-line cap; the shared setup lives in `common`.

mod common;

use core::num::NonZeroU32;

use common::*;
use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
    ids::{CalendarId, Uid},
    sync::SyncUpdate,
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
    Provider,
};

/// Creating a **recurring** event, and reading the rule back off the server.
///
/// The offline suite can only prove the body we build; the fixture-routing fake answers
/// canned bytes whatever it is sent (`AGENTS.md`). Only a real create says whether Graph
/// accepts the `patternedRecurrence` this adapter renders — and only a real read-back says
/// whether what came home is the rule that went out.
#[tokio::test]
async fn live_calendar_creates_a_recurring_event() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_creates_a_recurring_event: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let calendar_id = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
        .clone();
    let provider = calendar_provider(&token, calendar_id.clone());

    // Every Monday, eight times — the shape the product's repeat picker produces, and one
    // Graph states as `weekly` + `numbered` rather than as an RRULE.
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(8).unwrap());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uid = Uid::new(format!("live-recur-{unique}@allodia-e2e.test")).unwrap();
    let draft = EventDraft::new(
        calendar_id.clone(),
        uid,
        "provider-graph live recurrence probe",
        zoned("2026-09-07T09:30:00"),
        zoned("2026-09-07T10:00:00"),
        "2026-08-23T10:00:00Z".parse().unwrap(),
    )
    .description("safe to delete")
    .repeating(DraftRecurrence::new(rule.clone()));

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
    assert_eq!(
        stored.recurrence.as_ref().unwrap().rules,
        vec![rule],
        "the rule Graph stored is the rule that was sent"
    );

    let base = base_from(
        &calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}

/// Changing and removing a rule on Graph, where the pattern is structured and `null`
/// clears it.
///
/// ⚠️ This also pins the behaviour the product has to warn about: a rule change on Graph
/// discards every per-occurrence exception and cancellation. That is Outlook's own
/// semantics, measured rather than assumed (`calendar-semantics.md`).
#[tokio::test]
async fn live_calendar_changes_and_removes_a_rule() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_changes_and_removes_a_rule: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let calendar_id = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
        .clone();
    let provider = calendar_provider(&token, calendar_id.clone());

    let mut mondays = RecurrenceRule::new(Frequency::Weekly);
    mondays.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    mondays.bound = RecurrenceBound::Count(NonZeroU32::new(8).unwrap());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let draft = EventDraft::new(
        calendar_id.clone(),
        Uid::new(format!("live-rule-{unique}@allodia-e2e.test")).unwrap(),
        "provider-graph live rule-edit probe",
        zoned("2026-09-07T09:30:00"),
        zoned("2026-09-07T10:00:00"),
        "2026-08-23T10:00:00Z".parse().unwrap(),
    )
    .repeating(DraftRecurrence::new(mondays));
    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create a recurring event");

    // ---- Change the rule. ----
    let mut wednesdays = RecurrenceRule::new(Frequency::Weekly);
    wednesdays.by_day = vec![NDay {
        day: Weekday::We,
        nth_of_period: None,
    }];
    let base = base_from(
        &calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    let changed = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new("2026-08-23T10:05:00Z".parse().unwrap())
                    .recurrence(DraftRecurrence::new(wednesdays.clone())),
            ),
        )
        .await
        .expect("change the rule");

    let stored = |objects: &[engine_core::calendar::Event], id: &engine_core::ids::EventId| {
        objects
            .iter()
            .find(|e| &e.id == id)
            .expect("the series is in the snapshot")
            .clone()
    };
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    assert_eq!(
        stored(objects, &created.event).recurrence.unwrap().rules,
        vec![wednesdays],
        "Graph stored the new pattern"
    );

    // ---- Remove it: `null` turns the series into a single event. ----
    let base = base_from(
        &calendar_id,
        changed.event.as_str(),
        &changed.uid,
        changed.revisions.clone(),
    );
    let cleared = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new("2026-08-23T10:10:00Z".parse().unwrap()).clear_recurrence(),
            ),
        )
        .await
        .expect("remove the rule");

    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    assert!(
        !stored(objects, &created.event).is_recurring(),
        "the event no longer recurs"
    );

    let base = base_from(
        &calendar_id,
        cleared.event.as_str(),
        &cleared.uid,
        cleared.revisions.clone(),
    );
    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe event");
}

/// Removing **one occurrence** of a series, at the id Graph derives rather than one it was
/// handed.
///
/// ⚠️ **What this can prove, and what it cannot.** Graph reports a cancelled occurrence by
/// re-sending the series and its *surviving* occurrences — measured; there is no `@removed`
/// entry — and the reader keeps only masters and single events (`cal_fetch::keep`). So the
/// cancellation reaches nothing this suite can read, and it will keep being drawn until the
/// series' `cancelledOccurrences` is folded into its override map (`graph.md` → "Removing
/// one occurrence").
///
/// What is asserted here is the failure that would actually cost the user their data: the
/// derived id resolving to the **series**, taking every other occurrence with it. A wrong
/// *date* is caught offline, where the id is pinned as a string; a wrong *shape* would take
/// the whole event, and only a server can say.
#[tokio::test]
async fn live_calendar_removes_one_occurrence_and_keeps_the_series() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_removes_one_occurrence…: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let calendar_id = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
        .clone();
    let provider = calendar_provider(&token, calendar_id.clone());

    let mut mondays = RecurrenceRule::new(Frequency::Weekly);
    mondays.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    mondays.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar_id.clone(),
                Uid::new(format!("live-occ-{unique}@allodia-e2e.test")).unwrap(),
                "provider-graph live occurrence-delete probe",
                zoned("2026-09-07T09:30:00"),
                zoned("2026-09-07T10:00:00"),
                "2026-08-23T10:00:00Z".parse().unwrap(),
            )
            .repeating(DraftRecurrence::new(mondays)),
        )
        .await
        .expect("create a recurring event");

    let base = base_from(
        &calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    provider
        .delete_event(
            &account(),
            Some(&base),
            &EventDeletion::occurrence(
                &base,
                Occurrence::starting(zoned("2026-09-14T09:30:00")),
                "2026-08-23T10:05:00Z".parse().unwrap(),
            ),
        )
        .await
        .expect("remove one occurrence");

    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    let series = objects
        .iter()
        .find(|e| e.id == created.event)
        .expect("the series survived the removal of one of its occurrences");
    assert!(
        series.is_recurring(),
        "and it is still a series, with its rule intact"
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}
