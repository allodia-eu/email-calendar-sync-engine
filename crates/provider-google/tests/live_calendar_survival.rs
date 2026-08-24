//! Gated live check that Google's declared [`OverrideSurvival`] is still what Google does.
//!
//! The constant drives a warning a host shows the user *before* a series edit, and nothing
//! offline can tell a right answer from a wrong one: this is server policy. So the
//! experiment is re-run rather than remembered — create a series, give one occurrence its
//! own title at its own time, change one thing on the master, and ask the server what became
//! of the occurrence.
//!
//! Google is the only transport that answers **yes** to `clobbers_own_fields`, so this suite
//! is what keeps that surprising claim honest. Each flag gets its own series, because the
//! first destructive edit would otherwise decide the next one's starting state.

mod common;

use core::num::NonZeroU32;
use std::collections::BTreeMap;

use common::*;
use engine_core::{
    calendar::{Event, Frequency, RecurrenceBound, RecurrenceOverride, RecurrenceRule},
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    sync::SyncUpdate,
    time::LocalDateTime,
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
    Provider,
};
use provider_google::GoogleCalendarProvider;

const SERIES_START: &str = "2026-09-07T09:30:00";
const OVERRIDDEN: &str = "2026-09-14T09:30:00";
/// 09:30 Amsterdam on 14 September is 07:30Z — CEST, and the resolution the caller owns.
const OVERRIDDEN_UTC: &str = "2026-09-14T07:30:00Z";
const OVERRIDE_TITLE: &str = "Renamed by hand";

#[tokio::test]
async fn live_calendar_override_survival_is_what_the_adapter_advertises() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_override_survival…: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let declared = provider
        .connection_info()
        .capabilities
        .override_survival()
        .expect("an adapter that writes calendars states what a series edit costs");

    // Renaming the series: does the occurrence keep the name the user gave it?
    let renamed = measure(&provider, "title", |_| {
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
    let moved = measure(&provider, "time", |_| {
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
    // override would mean destroyed rather than merely unscheduled.
    let mut shorter = RecurrenceRule::new(Frequency::Weekly);
    shorter.bound = RecurrenceBound::Count(NonZeroU32::new(4).unwrap());
    let ruled = measure(&provider, "rule", move |_| {
        EventPatch::new("2026-08-24T10:05:00Z".parse().unwrap())
            .recurrence(DraftRecurrence::new(shorter.clone()))
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
async fn measure<F>(provider: &GoogleCalendarProvider, label: &str, edit: F) -> Outcome
where
    F: FnOnce(&Event) -> EventPatch,
{
    let (base, id) = series_with_an_override(provider, label).await;
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(&base, PatchTarget::Series, edit(&base)),
        )
        .await
        .expect("edit the series");

    let overrides = overrides_of(provider, &id).await;
    let entry = overrides.get(&OVERRIDDEN.parse::<LocalDateTime>().unwrap());
    let outcome = Outcome {
        survived: entry.is_some(),
        clobbered: match entry {
            Some(RecurrenceOverride::Patch(patch)) => {
                patch.get("title").and_then(serde_json::Value::as_str) != Some(OVERRIDE_TITLE)
            }
            _ => false,
        },
    };

    // Unguarded: the edit under test moved the series' ETag, and the cleanup is not the
    // thing being measured.
    provider
        .delete_event(
            &account(),
            None,
            &EventDeletion::unconditional(id, base.uid.clone()),
        )
        .await
        .expect("delete the probe series");
    outcome
}

/// A fresh weekly series whose second occurrence has been given its own title, at its own
/// time. Returns the base to guard the next write with, and the series' id.
async fn series_with_an_override(
    provider: &GoogleCalendarProvider,
    label: &str,
) -> (Event, EventId) {
    let cal = CalendarId::try_from("primary").unwrap();
    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                cal.clone(),
                Uid::new(format!("live-survival-{label}-{unique}@example.test")).unwrap(),
                "Live survival probe",
                zoned(SERIES_START),
                zoned("2026-09-07T10:00:00"),
                "2026-08-24T10:00:00Z".parse().unwrap(),
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    let base = base_of(
        &cal,
        &created.event,
        &created.uid,
        created.revisions.clone(),
    );
    let overridden = provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::at(
                    zoned(OVERRIDDEN),
                    OVERRIDDEN_UTC.parse().unwrap(),
                )),
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
            .contains_key(&OVERRIDDEN.parse::<LocalDateTime>().unwrap()),
        "the {label} experiment starts from an override that is actually there"
    );

    // Overriding an occurrence moved the series' own ETag, so the next edit guards on the
    // newest revision rather than the create's.
    let base = base_of(&cal, &created.event, &created.uid, overridden.revisions);
    (base, created.event)
}

/// A minimal event carrying the identity + revision a write receipt reports.
fn base_of(
    calendar: &CalendarId,
    id: &EventId,
    uid: &Uid,
    revisions: engine_core::version::RevisionTokens,
) -> Event {
    let mut event = Event::new(
        id.clone(),
        uid.clone(),
        Memberships::of_one(calendar.clone()),
        zoned(SERIES_START),
    );
    event.revisions = revisions;
    event
}

/// The override map of one series, read back through the adapter's own sync path.
async fn overrides_of(
    provider: &GoogleCalendarProvider,
    id: &EventId,
) -> BTreeMap<LocalDateTime, RecurrenceOverride> {
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
