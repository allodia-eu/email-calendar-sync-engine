//! Gated live check that JMAP's declared [`OverrideSurvival`] is still what Stalwart does.
//!
//! The adapter claims a series edit costs the user nothing, and the reason is structural — a
//! `/set` takes a `PatchObject`, so an edit names the master's own properties and
//! `recurrenceOverrides` is not among them. Structural is not the same as true: the *server*
//! decides what a patch does, and this is what says it still does what it did. Skips with no
//! `STALWART_HTTP_ADDR`.
//!
//! Each of the three flags gets its own series, because the first edit would otherwise
//! decide the next one's starting state.

mod common;

use core::num::NonZeroU32;
use std::collections::BTreeMap;

use common::*;
use engine_core::{
    calendar::{Frequency, RecurrenceBound, RecurrenceOverride, RecurrenceRule},
    ids::Uid,
    time::LocalDateTime,
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
    Provider,
};
use provider_jmap::JmapProvider;

const SERIES_START: &str = "2026-06-01T09:30:00";
const OVERRIDDEN: &str = "2026-06-08T09:30:00";
const OVERRIDE_TITLE: &str = "Renamed by hand";

#[tokio::test]
async fn override_survival_is_what_the_adapter_advertises() {
    let Some(provider) = setup("override_survival_is_what_the_adapter_advertises").await else {
        return;
    };
    let declared = provider
        .connection_info()
        .capabilities
        .override_survival()
        .expect("an adapter that writes calendars states what a series edit costs");

    let renamed = measure(&provider, "title", |patch| patch.summary("Series renamed")).await;
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

    let moved = measure(&provider, "time", |patch| {
        patch
            .start(amsterdam("2026-06-01T11:00:00"))
            .end(amsterdam("2026-06-01T11:30:00"))
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

    // A rule that still produces the overridden date, so a missing override would mean
    // destroyed rather than merely unscheduled.
    let mut shorter = RecurrenceRule::new(Frequency::Weekly);
    shorter.bound = RecurrenceBound::Count(NonZeroU32::new(4).unwrap());
    let ruled = measure(&provider, "rule", move |patch| {
        patch.recurrence(DraftRecurrence::new(shorter.clone()))
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
    survived: bool,
    clobbered: bool,
}

/// Builds a series with an override, applies `edit` to the master, and reads it back.
async fn measure<F>(provider: &JmapProvider, label: &str, edit: F) -> Outcome
where
    F: FnOnce(EventPatch) -> EventPatch,
{
    let uid = format!("live-jmap-survival-{label}@test.local");
    let base = series_with_an_override(provider, &uid).await;
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(&base, PatchTarget::Series, edit(EventPatch::new(stamp()))),
        )
        .await
        .expect("edit the series");

    let overrides = overrides_of(provider, &uid).await;
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

    let stored = require(provider, &uid).await;
    provider
        .delete_event(&account(), None, &EventDeletion::of(&stored))
        .await
        .expect("delete the probe series");
    outcome
}

/// A fresh weekly series whose second occurrence has its own title, at its own time.
async fn series_with_an_override(
    provider: &JmapProvider,
    uid: &str,
) -> engine_core::calendar::Event {
    pre_clean(provider, uid).await;
    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(provider).await,
                Uid::new(uid).unwrap(),
                "Live JMAP survival probe",
                amsterdam(SERIES_START),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    let base = require(provider, uid).await;
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::starting(amsterdam(OVERRIDDEN))),
                EventPatch::new(stamp())
                    .summary(OVERRIDE_TITLE)
                    .start(amsterdam("2026-06-08T14:00:00"))
                    .end(amsterdam("2026-06-08T14:45:00")),
            ),
        )
        .await
        .expect("override one occurrence");
    assert!(
        overrides_of(provider, uid)
            .await
            .contains_key(&OVERRIDDEN.parse::<LocalDateTime>().unwrap()),
        "the experiment starts from an override that is actually there"
    );
    require(provider, uid).await
}

/// The override map of one series, read back through the adapter's own sync path.
async fn overrides_of(
    provider: &JmapProvider,
    uid: &str,
) -> BTreeMap<LocalDateTime, RecurrenceOverride> {
    require(provider, uid)
        .await
        .recurrence
        .map(|r| r.overrides)
        .unwrap_or_default()
}
