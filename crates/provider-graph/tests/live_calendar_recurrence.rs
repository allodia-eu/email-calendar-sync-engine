//! Gated live checks for **recurring** calendar writes over Microsoft Graph: creating a
//! series, changing and removing its rule, and removing one occurrence of it.
//!
//! Its own file rather than an addition to `live_calendar.rs`, which is close to the
//! 500-line cap; the shared setup lives in `common`.

mod common;

use core::num::NonZeroU32;

use common::*;
use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceOverride, RecurrenceRule, Weekday},
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
/// handed — and reading the removal back.
///
/// Two failures only a server can show. The derived id could resolve to the **series**,
/// taking every other occurrence with it — a wrong *date* is caught offline, where the id
/// is pinned as a string, but a wrong *shape* takes the whole event. And the removal has to
/// come home: Graph reports it by re-sending the series and its *surviving* occurrences,
/// with no `@removed` entry anywhere (measured), so the only thing that says the occurrence
/// is gone is the master's own `cancelledOccurrences`.
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
    let overrides = &series.recurrence.as_ref().expect("a rule").overrides;
    assert!(
        matches!(
            overrides.get(&"2026-09-14T09:30:00".parse().unwrap()),
            Some(RecurrenceOverride::Excluded)
        ),
        "the removed occurrence must stop being drawn: {:?}",
        overrides.keys().collect::<Vec<_>>()
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}

/// Editing **one occurrence** of a series, at the id Graph derives for it — and reading the
/// edit back.
///
/// Same shape as the removal beside it. The failure that would cost the user their series is
/// an id resolving to the **master**, so that renaming one Monday renames every Monday; the
/// id's *date* is pinned offline as a string, but only a server can say what it resolves to.
/// The read-back has its own trap: once patched, Graph gives the occurrence an **opaque**
/// id, so the date it keys on can only come from the `occurrenceId` it keeps beside it.
#[tokio::test]
async fn live_calendar_edits_one_occurrence_and_leaves_the_series_alone() {
    const TITLE: &str = "provider-graph live occurrence-edit probe";

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_edits_one_occurrence…: GRAPH_ACCESS_TOKEN unset");
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
                Uid::new(format!("live-occ-edit-{unique}@allodia-e2e.test")).unwrap(),
                TITLE,
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
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::starting(zoned("2026-09-14T09:30:00"))),
                EventPatch::new("2026-08-23T10:05:00Z".parse().unwrap())
                    .summary("Moved to the afternoon"),
            ),
        )
        .await
        .expect("edit one occurrence");

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
        .expect("the series survived the edit of one of its occurrences");
    assert_eq!(
        series.title, TITLE,
        "the series keeps its own title — an id that resolved to it would have renamed \
         every occurrence"
    );
    assert!(series.is_recurring(), "and it is still a series");
    let overrides = &series.recurrence.as_ref().expect("a rule").overrides;
    let Some(RecurrenceOverride::Patch(patch)) =
        overrides.get(&"2026-09-14T09:30:00".parse().unwrap())
    else {
        panic!(
            "the edited occurrence must come home as an override of its series: {:?}",
            overrides.keys().collect::<Vec<_>>()
        );
    };
    assert_eq!(
        patch.get("title").and_then(serde_json::Value::as_str),
        Some("Moved to the afternoon")
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}
