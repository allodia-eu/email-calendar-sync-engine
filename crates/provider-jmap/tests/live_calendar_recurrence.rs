//! Gated live checks for **creating a recurring event** over JMAP, against the Stalwart
//! harness. Skips with no `STALWART_HTTP_ADDR`.
//!
//! Its own file rather than an addition to `live_calendar_write.rs`, which is close to the
//! 500-line cap; the shared setup lives in `common`.

mod common;

use common::*;
use engine_core::ids::Uid;
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget, Provider,
};

/// Creating a **recurring** event through the neutral draft, and reading the rule back.
///
/// The offline fake replies `created` to any object at all, so only a real server says
/// whether the JSCalendar `recurrenceRules` this adapter renders is one Stalwart accepts.
#[tokio::test]
async fn create_carries_a_recurrence_rule() {
    use core::num::NonZeroU32;

    use engine_core::calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};
    use engine_provider::DraftRecurrence;

    const RECURRING_UID: &str = "live-jmap-recurring@test.local";

    let Some(provider) = setup("create_carries_a_recurrence_rule").await else {
        return;
    };
    pre_clean(&provider, RECURRING_UID).await;

    // Every Monday, eight times — the shape the product's repeat picker produces.
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(8).unwrap());

    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(RECURRING_UID).unwrap(),
                "Live JMAP recurring write test",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(rule.clone())),
        )
        .await
        .expect("create a recurring event");

    let made = require(&provider, RECURRING_UID).await;
    assert!(
        made.is_recurring(),
        "the created event came back as a series master"
    );
    assert_eq!(
        made.recurrence.as_ref().unwrap().rules,
        vec![rule],
        "the rule the server stored is the rule that was sent"
    );

    let mut base = made.clone();
    base.revisions = created.revisions.clone();
    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}

/// Changing and removing the rule, server-side.
///
/// The offline fake answers `updated` to any patch at all, so only a real server says
/// whether the singular `recurrenceRule` pointer — and the `null` that removes it — is
/// one Stalwart acts on.
#[tokio::test]
async fn a_rule_can_be_changed_and_removed() {
    use core::num::NonZeroU32;

    use engine_core::calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};
    use engine_provider::DraftRecurrence;

    const UID: &str = "live-jmap-rule-edit@test.local";

    let Some(provider) = setup("a_rule_can_be_changed_and_removed").await else {
        return;
    };
    pre_clean(&provider, UID).await;

    let mut mondays = RecurrenceRule::new(Frequency::Weekly);
    mondays.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    mondays.bound = RecurrenceBound::Count(NonZeroU32::new(8).unwrap());

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(UID).unwrap(),
                "Live JMAP rule edit",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(mondays)),
        )
        .await
        .expect("create a recurring event");

    // ---- Change it to every Wednesday. ----
    let mut wednesdays = RecurrenceRule::new(Frequency::Weekly);
    wednesdays.by_day = vec![NDay {
        day: Weekday::We,
        nth_of_period: None,
    }];
    let base = require(&provider, UID).await;
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).recurrence(DraftRecurrence::new(wednesdays.clone())),
            ),
        )
        .await
        .expect("change the rule");
    assert_eq!(
        require(&provider, UID).await.recurrence.unwrap().rules,
        vec![wednesdays],
        "the server stored the new rule"
    );

    // ---- Remove it. ----
    let base = require(&provider, UID).await;
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).clear_recurrence(),
            ),
        )
        .await
        .expect("remove the rule");
    let single = require(&provider, UID).await;
    assert!(
        !single.is_recurring(),
        "the event no longer recurs: {:?}",
        single.recurrence
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&single))
        .await
        .expect("delete the probe event");
}

/// Editing an occurrence of a series **nobody has overridden yet**, then editing it again.
///
/// The two shapes, in the only order that can tell them apart. RFC 8620 §5.3 lets a JSON
/// pointer address only what already exists, so on a series with no `recurrenceOverrides`
/// the pointer form is rejected *whole* — the edit is lost, not degraded. Nothing offline
/// can see that: the fake answers `updated` to any object it is handed.
#[tokio::test]
async fn a_first_edit_of_an_occurrence_lands_and_so_does_the_second() {
    use engine_core::calendar::{Frequency, RecurrenceOverride, RecurrenceRule};
    use engine_provider::DraftRecurrence;

    const UID: &str = "live-jmap-first-override@test.local";
    const OCCURRENCE: &str = "2026-06-08T09:30:00";

    let Some(provider) = setup("a_first_edit_of_an_occurrence_lands_and_so_does_the_second").await
    else {
        return;
    };
    pre_clean(&provider, UID).await;

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(UID).unwrap(),
                "Live first-override probe",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(RecurrenceRule::new(Frequency::Weekly))),
        )
        .await
        .expect("create a recurring event");

    let title_at = |event: &engine_core::calendar::Event| -> Option<String> {
        let RecurrenceOverride::Patch(patch) = event
            .recurrence
            .as_ref()?
            .overrides
            .get(&OCCURRENCE.parse().unwrap())?
        else {
            return None;
        };
        patch
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };

    let series = require(&provider, UID).await;
    assert!(
        series
            .recurrence
            .as_ref()
            .is_some_and(|r| r.overrides.is_empty()),
        "the series starts with no overrides at all — the state the two shapes differ on"
    );

    // ---- First edit: there is no map for a pointer to address. ----
    provider
        .patch_event(
            &account(),
            &series,
            &EventEdit::new(
                &series,
                PatchTarget::Instance(Occurrence::starting(amsterdam(OCCURRENCE))),
                EventPatch::new(stamp()).summary("Moved to the afternoon"),
            ),
        )
        .await
        .expect("the server accepts the first override of an occurrence");
    let first = require(&provider, UID).await;
    assert_eq!(
        title_at(&first).as_deref(),
        Some("Moved to the afternoon"),
        "the override the server materialized carries the new title"
    );

    // ---- Second edit: now it does. ----
    provider
        .patch_event(
            &account(),
            &first,
            &EventEdit::new(
                &first,
                PatchTarget::Instance(Occurrence::starting(amsterdam(OCCURRENCE))),
                EventPatch::new(stamp()).summary("Renamed again"),
            ),
        )
        .await
        .expect("the server accepts a pointer into the override it now holds");
    let second = require(&provider, UID).await;
    assert_eq!(title_at(&second).as_deref(), Some("Renamed again"));
    assert_eq!(
        second.recurrence.as_ref().unwrap().overrides.len(),
        1,
        "and the second edit patched that entry rather than adding another"
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&second))
        .await
        .expect("delete the probe event");
}

/// Removing **one occurrence**, from a series with no overrides and then from one with.
///
/// Both shapes again, and for the same reason as the first edit: with no
/// `recurrenceOverrides` on the event there is nothing for a pointer to address, so the
/// exclusion has to assign the map. And the second removal proves the pointer form reaches
/// the same place — which the offline fake, answering `updated` to anything, cannot.
#[tokio::test]
async fn an_occurrence_can_be_removed() {
    use engine_core::calendar::{Frequency, RecurrenceOverride, RecurrenceRule};
    use engine_provider::DraftRecurrence;

    const UID: &str = "live-jmap-occurrence-delete@test.local";

    let Some(provider) = setup("an_occurrence_can_be_removed").await else {
        return;
    };
    pre_clean(&provider, UID).await;

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(UID).unwrap(),
                "Live occurrence delete",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(RecurrenceRule::new(Frequency::Weekly))),
        )
        .await
        .expect("create a recurring event");

    for removed in ["2026-06-08T09:30:00", "2026-06-15T09:30:00"] {
        let base = require(&provider, UID).await;
        provider
            .delete_event(
                &account(),
                Some(&base),
                &EventDeletion::occurrence(
                    &base,
                    Occurrence::starting(amsterdam(removed)),
                    stamp(),
                ),
            )
            .await
            .expect("the server accepts the exclusion");
    }

    let after = require(&provider, UID).await;
    let recurrence = after.recurrence.as_ref().expect("still a series");
    for removed in ["2026-06-08T09:30:00", "2026-06-15T09:30:00"] {
        assert_eq!(
            recurrence.overrides.get(&removed.parse().unwrap()),
            Some(&RecurrenceOverride::Excluded),
            "{removed} is excluded: {:?}",
            recurrence.overrides
        );
    }
    assert!(!recurrence.rules.is_empty(), "the rule itself survives");

    provider
        .delete_event(&account(), None, &EventDeletion::of(&after))
        .await
        .expect("delete the probe event");
}

/// Removing an occurrence the user had **edited** replaces that override rather than
/// merging into it: an excluded override may carry nothing else (RFC 8984 §4.3.3).
#[tokio::test]
async fn removing_an_edited_occurrence_replaces_its_override() {
    use engine_core::calendar::{Frequency, RecurrenceOverride, RecurrenceRule};
    use engine_provider::DraftRecurrence;

    const UID: &str = "live-jmap-edited-occurrence-delete@test.local";
    const OCCURRENCE: &str = "2026-06-08T09:30:00";

    let Some(provider) = setup("removing_an_edited_occurrence_replaces_its_override").await else {
        return;
    };
    pre_clean(&provider, UID).await;

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(UID).unwrap(),
                "Live edited-occurrence delete",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(RecurrenceRule::new(Frequency::Weekly))),
        )
        .await
        .expect("create a recurring event");

    let base = require(&provider, UID).await;
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::starting(amsterdam(OCCURRENCE))),
                EventPatch::new(stamp()).summary("Moved to the afternoon"),
            ),
        )
        .await
        .expect("override one occurrence");

    let base = require(&provider, UID).await;
    provider
        .delete_event(
            &account(),
            Some(&base),
            &EventDeletion::occurrence(&base, Occurrence::starting(amsterdam(OCCURRENCE)), stamp()),
        )
        .await
        .expect("remove the occurrence the user had edited");

    let after = require(&provider, UID).await;
    assert_eq!(
        after
            .recurrence
            .as_ref()
            .expect("a series")
            .overrides
            .get(&OCCURRENCE.parse().unwrap()),
        Some(&RecurrenceOverride::Excluded),
        "the edit is gone with the occurrence, not left beside the exclusion"
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&after))
        .await
        .expect("delete the probe event");
}
