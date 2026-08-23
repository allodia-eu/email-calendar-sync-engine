//! Gated live checks for **creating a recurring event** over JMAP, against the Stalwart
//! harness. Skips with no `STALWART_HTTP_ADDR`.
//!
//! Its own file rather than an addition to `live_calendar_write.rs`, which is close to the
//! 500-line cap; the shared setup lives in `common`.

mod common;

use common::*;
use engine_core::ids::Uid;
use engine_provider::{EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget, Provider};

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
        .delete_event(&account(), &EventDeletion::of(&base))
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
        .delete_event(&account(), &EventDeletion::of(&single))
        .await
        .expect("delete the probe event");
}
