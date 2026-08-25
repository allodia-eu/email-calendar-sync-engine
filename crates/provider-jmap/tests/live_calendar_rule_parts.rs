//! Gated live check that every JSCalendar `RecurrenceRule` part this adapter can express
//! survives a round trip through a real Stalwart server. Skips with no `STALWART_HTTP_ADDR`.
//!
//! Its own file rather than an addition to `live_calendar_recurrence.rs`, which is close to
//! the 500-line cap; the shared setup lives in `common`.
//!
//! # Why a live test, when the offline round-trip test already passes
//!
//! The offline test renders a rule and reads it straight back, so it proves this crate agrees
//! with itself and nothing more — the JSON never leaves the process. Six parts were dropped on
//! read for as long as they had been rendered on write, and no offline test noticed, because
//! the fake executor answers canned bytes whatever it is sent (`AGENTS.md`). Only a real
//! server says whether it *stores* `bySetPosition` and hands it back, which is the claim the
//! parser's correctness actually rests on.

mod common;

use common::*;
use engine_core::ids::Uid;
use engine_provider::{EventDeletion, EventDraft, Provider};

/// A rule carrying the parts that decide *which dates* it generates comes back carrying them.
///
/// `bySetPosition` is the one with teeth. Dropped on read, "the fourth Wednesday of the month"
/// arrives as "every Wednesday" — a rule the expander is willing to expand, onto dates the
/// user never asked for. Carried, it reaches `check_supported`, which refuses it, and the
/// event is reported as unexpandable instead of being drawn wrong.
#[tokio::test]
async fn a_rule_keeps_the_parts_that_decide_its_dates() {
    use core::num::NonZeroU32;

    use engine_core::calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};
    use engine_provider::DraftRecurrence;

    const UID: &str = "live-jmap-rule-parts@test.local";

    let Some(provider) = setup("a_rule_keeps_the_parts_that_decide_its_dates").await else {
        return;
    };
    pre_clean(&provider, UID).await;

    // The fourth Wednesday of every month, twelve times. Without `by_set_position` this is
    // "every Wednesday" — same frequency, same byDay, a different event.
    let mut rule = RecurrenceRule::new(Frequency::Monthly);
    rule.by_day = vec![NDay {
        day: Weekday::We,
        nth_of_period: None,
    }];
    rule.by_set_position = vec![4];
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(12).unwrap());

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(UID).unwrap(),
                "Live JMAP rule parts",
                amsterdam("2026-06-24T09:30:00"),
                amsterdam("2026-06-24T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(rule.clone())),
        )
        .await
        .expect("create a series with a bySetPosition rule");

    let made = require(&provider, UID).await;
    let stored = &made
        .recurrence
        .as_ref()
        .expect("the series has a rule")
        .rules[0];
    assert_eq!(
        stored.by_set_position,
        vec![4],
        "the server stored bySetPosition and this adapter read it back; dropping it on read \
         turns the fourth Wednesday into every Wednesday, silently"
    );
    assert_eq!(
        stored, &rule,
        "the whole rule came back as it was sent, not merely its bySetPosition"
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&made))
        .await
        .expect("delete the probe series");
}
