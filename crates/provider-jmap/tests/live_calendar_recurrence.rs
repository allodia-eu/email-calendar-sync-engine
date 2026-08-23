//! Gated live checks for **creating a recurring event** over JMAP, against the Stalwart
//! harness. Skips with no `STALWART_HTTP_ADDR`.
//!
//! Its own file rather than an addition to `live_calendar_write.rs`, which is close to the
//! 500-line cap; the shared setup lives in `common`.

mod common;

use common::*;
use engine_core::ids::Uid;
use engine_provider::{EventDeletion, EventDraft, Provider};

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
