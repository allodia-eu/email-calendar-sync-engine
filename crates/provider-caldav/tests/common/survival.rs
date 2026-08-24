//! Live CalDAV scenario: what a **series-level** edit does to an occurrence the user
//! changed on its own.
//!
//! The adapter claims a series edit costs nothing, and its reason is structural — the
//! patcher rewrites only the master `VEVENT`'s own lines, so a `RECURRENCE-ID` component is
//! untouched by construction. Structural is not the same as true: the *server* stores the
//! document and is free to reserialize, merge or drop parts of it, and Stalwart demonstrably
//! reserializes what it stores ([`recurrence`](super::recurrence)). So the claim is
//! re-measured rather than reasoned about, on **both** harness servers — a policy one of
//! them has and the other does not is exactly what a single-server test would miss.
//!
//! Its own file rather than an addition to [`recurrence`](super::recurrence), which is
//! already long, and because this asks a different question: not "did the write land" but
//! "what did it cost".

use core::num::NonZeroU32;

use engine_core::{
    calendar::{Event, Frequency, RecurrenceBound, RecurrenceOverride, RecurrenceRule},
    ids::{AccountId, Uid},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
    Provider,
};
use provider_caldav::CalDavProvider;

use super::{pre_clean, require};

const SURVIVAL_UID: &str = "live-caldav-survival@test.local";
const SERIES_START: &str = "2026-06-01T09:30:00";
const OVERRIDDEN: &str = "2026-06-08T09:30:00";
const OVERRIDE_TITLE: &str = "Renamed by hand";

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// Runs the three experiments and asserts each against what the adapter advertises.
pub(crate) async fn survival_is_what_the_adapter_advertises(
    provider: &CalDavProvider,
    account: &AccountId,
) {
    let declared = provider
        .connection_info()
        .capabilities
        .override_survival()
        .expect("an adapter that writes calendars states what a series edit costs");

    let renamed = measure(provider, account, |patch| patch.summary("Series renamed")).await;
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

    let moved = measure(provider, account, |patch| {
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
    let ruled = measure(provider, account, move |patch| {
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
async fn measure<F>(provider: &CalDavProvider, account: &AccountId, edit: F) -> Outcome
where
    F: FnOnce(EventPatch) -> EventPatch,
{
    let base = series_with_an_override(provider, account).await;
    provider
        .patch_event(
            account,
            &base,
            &EventEdit::new(&base, PatchTarget::Series, edit(EventPatch::new(stamp()))),
        )
        .await
        .expect("edit the series");

    let stored = require(provider, account, SURVIVAL_UID).await;
    let entry = stored.recurrence.as_ref().and_then(|r| {
        r.overrides
            .get(&OVERRIDDEN.parse::<LocalDateTime>().unwrap())
    });
    let outcome = Outcome {
        survived: entry.is_some(),
        clobbered: match entry {
            Some(RecurrenceOverride::Patch(patch)) => {
                patch.get("title").and_then(serde_json::Value::as_str) != Some(OVERRIDE_TITLE)
            }
            _ => false,
        },
    };

    provider
        .delete_event(account, None, &EventDeletion::of(&stored))
        .await
        .expect("delete the probe series");
    outcome
}

/// A fresh weekly series whose second occurrence has its own title, at its own time.
async fn series_with_an_override(provider: &CalDavProvider, account: &AccountId) -> Event {
    let uid = Uid::new(SURVIVAL_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());
    provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid,
                "Live CalDAV survival probe",
                amsterdam(SERIES_START),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    let base = require(provider, account, SURVIVAL_UID).await;
    provider
        .patch_event(
            account,
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

    let with_override = require(provider, account, SURVIVAL_UID).await;
    assert!(
        with_override
            .recurrence
            .as_ref()
            .is_some_and(|r| r.overrides.contains_key(&OVERRIDDEN.parse().unwrap())),
        "the experiment starts from an override that is actually there"
    );
    with_override
}
