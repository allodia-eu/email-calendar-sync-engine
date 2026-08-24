//! Gated live check that Graph's declared [`OverrideSurvival`] is still what Graph does.
//!
//! The constant drives a warning a host shows the user *before* a series edit, and nothing
//! offline can tell a right answer from a wrong one: this is server policy, and Exchange is
//! free to change it. So the experiment is re-run rather than remembered — create a series,
//! give one occurrence its own title at its own time, change one thing on the master, and
//! ask the server what became of the occurrence.
//!
//! Each of the three flags gets its **own** series, because the first destructive edit would
//! otherwise decide the next one's starting state.

mod common;

use core::num::NonZeroU32;

use common::*;
use engine_core::{
    calendar::{
        Event, Frequency, NDay, RecurrenceBound, RecurrenceOverride, RecurrenceRule, Weekday,
    },
    ids::{CalendarId, Uid},
    sync::SyncUpdate,
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
    Provider,
};
use provider_graph::GraphCalendarProvider;

/// The series starts here and repeats weekly; the second Monday is the one overridden.
const SERIES_START: &str = "2026-09-07T09:30:00";
const OVERRIDDEN: &str = "2026-09-14T09:30:00";
const OVERRIDE_TITLE: &str = "Renamed by hand";

#[tokio::test]
async fn live_calendar_override_survival_is_what_the_adapter_advertises() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_override_survival…: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let calendar_id = default_calendar(&token).await;
    let provider = calendar_provider(&token, calendar_id.clone());
    let declared = provider
        .connection_info()
        .capabilities
        .override_survival()
        .expect("an adapter that writes calendars states what a series edit costs");

    // Renaming the series: does the occurrence keep the name the user gave it?
    let renamed = measure(&provider, &calendar_id, "title", |_| {
        EventPatch::new("2026-08-24T10:05:00Z".parse().unwrap()).summary("Series renamed")
    })
    .await;
    assert_eq!(
        renamed.clobbered,
        declared.clobbers_own_fields,
        "clobbers_own_fields is wrong: the occurrence's own title {} the series rename",
        if renamed.clobbered {
            "did not survive"
        } else {
            "survived"
        }
    );

    // Moving the series' time: does the override survive at all?
    let moved = measure(&provider, &calendar_id, "time", |_| {
        EventPatch::new("2026-08-24T10:05:00Z".parse().unwrap())
            .start(zoned("2026-09-07T11:00:00"))
            .end(zoned("2026-09-07T11:30:00"))
    })
    .await;
    assert_eq!(
        moved.survived,
        declared.survives_time_change,
        "survives_time_change is wrong: after moving the series, the override {}",
        if moved.survived {
            "was there"
        } else {
            "was gone"
        }
    );

    // Changing the rule — to one that still produces the overridden date, so a missing
    // override means destroyed rather than merely unscheduled.
    let mut every_two_weeks = RecurrenceRule::new(Frequency::Weekly);
    every_two_weeks.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    every_two_weeks.bound = RecurrenceBound::Count(NonZeroU32::new(4).unwrap());
    let ruled = measure(&provider, &calendar_id, "rule", move |_| {
        EventPatch::new("2026-08-24T10:05:00Z".parse().unwrap())
            .recurrence(DraftRecurrence::new(every_two_weeks.clone()))
    })
    .await;
    assert_eq!(
        ruled.survived,
        declared.survives_rule_change,
        "survives_rule_change is wrong: after changing the rule, the override {}",
        if ruled.survived {
            "was there"
        } else {
            "was gone"
        }
    );
}

/// What became of the overridden occurrence after the master was edited.
struct Outcome {
    /// The override was still in the series' map.
    survived: bool,
    /// It was there, but the title the user gave it had been overwritten.
    clobbered: bool,
}

/// Runs one experiment end to end: build a series, override its second occurrence, apply
/// `edit` to the master, and read the occurrence back.
async fn measure<F>(
    provider: &GraphCalendarProvider,
    calendar_id: &CalendarId,
    label: &str,
    edit: F,
) -> Outcome
where
    F: FnOnce(&Event) -> EventPatch,
{
    let (base, created_id) = series_with_an_override(provider, calendar_id, label).await;
    let patched = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(&base, PatchTarget::Series, edit(&base)),
        )
        .await
        .expect("edit the series");

    let overrides = overrides_of(provider, &created_id).await;
    let entry = overrides.get(&OVERRIDDEN.parse().unwrap());
    let outcome = Outcome {
        survived: entry.is_some(),
        clobbered: match entry {
            Some(RecurrenceOverride::Patch(patch)) => {
                patch.get("title").and_then(serde_json::Value::as_str) != Some(OVERRIDE_TITLE)
            }
            _ => false,
        },
    };

    let cleanup = base_from(
        calendar_id,
        created_id.as_str(),
        &base.uid,
        patched.revisions,
    );
    provider
        .delete_event(&account(), None, &EventDeletion::of(&cleanup))
        .await
        .expect("delete the probe series");
    outcome
}

/// A fresh weekly series whose second occurrence has been given its own title, at its own
/// time. Returns the base to guard the next write with, and the series' id.
async fn series_with_an_override(
    provider: &GraphCalendarProvider,
    calendar_id: &CalendarId,
    label: &str,
) -> (Event, engine_core::ids::EventId) {
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
                Uid::new(format!("live-survival-{label}-{unique}@allodia-e2e.test")).unwrap(),
                "provider-graph live survival probe",
                zoned(SERIES_START),
                zoned("2026-09-07T10:00:00"),
                "2026-08-24T10:00:00Z".parse().unwrap(),
            )
            .repeating(DraftRecurrence::new(mondays)),
        )
        .await
        .expect("create a recurring event");

    let base = base_from(
        calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    let overridden = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::starting(zoned(OVERRIDDEN))),
                EventPatch::new("2026-08-24T10:01:00Z".parse().unwrap())
                    .summary(OVERRIDE_TITLE)
                    .start(zoned("2026-09-14T14:00:00"))
                    .end(zoned("2026-09-14T14:45:00")),
            ),
        )
        .await
        .expect("override one occurrence");
    assert!(
        overrides_of(provider, &created.event)
            .await
            .contains_key(&OVERRIDDEN.parse().unwrap()),
        "the {label} experiment starts from an override that is actually there"
    );

    // The series' own revision moved when the occurrence was written, so the next edit
    // guards on the newest one rather than the create's.
    let base = base_from(
        calendar_id,
        created.event.as_str(),
        &created.uid,
        overridden.revisions,
    );
    (base, created.event)
}

/// The override map of one series, read back through the adapter's own sync path.
async fn overrides_of(
    provider: &GraphCalendarProvider,
    id: &engine_core::ids::EventId,
) -> std::collections::BTreeMap<engine_core::time::LocalDateTime, RecurrenceOverride> {
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    objects
        .iter()
        .find(|e| &e.id == id)
        .and_then(|e| e.recurrence.as_ref())
        .map(|r| r.overrides.clone())
        .unwrap_or_default()
}

/// The account's default calendar.
async fn default_calendar(token: &str) -> CalendarId {
    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
        .clone()
}
