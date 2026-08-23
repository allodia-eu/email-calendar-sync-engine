//! Live CalDAV scenario: creating a **recurring** event, and reading the rule back.
//!
//! Its own file rather than an addition to [`write`](super::write), which is already close
//! to the 500-line cap.
//!
//! What only a real server can settle: the offline suite proves the `RRULE` line we build,
//! but `MockStream` answers canned bytes whatever it is sent (`AGENTS.md`), so nothing
//! offline says Stalwart accepts the document. And Stalwart **reserializes** what it stores
//! — it reorders `RRULE` parts — so a read-back is the only evidence the rule survived as a
//! *rule* rather than as bytes.

use core::num::NonZeroU32;

use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
    ids::{AccountId, Uid},
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{DraftRecurrence, EventDeletion, EventDraft, Provider};
use provider_caldav::CalDavProvider;

use super::{pre_clean, require};

const RECURRING_UID: &str = "live-caldav-recurring@test.local";

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// Every Monday, eight times — the shape the product's repeat picker produces.
fn weekly_on_monday() -> RecurrenceRule {
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(8).unwrap());
    rule
}

/// Creates a recurring event through the neutral draft, then reads the rule back off the
/// server and deletes it. Leaves the seed untouched.
pub(crate) async fn create_carries_the_rule(provider: &CalDavProvider, account: &AccountId) {
    let uid = Uid::new(RECURRING_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    let created = provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid.clone(),
                "Live recurring write test",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(weekly_on_monday())),
        )
        .await
        .expect("create a recurring event");

    let made = require(provider, account, RECURRING_UID).await;
    assert!(
        made.is_recurring(),
        "the created event came back as a series master"
    );
    assert_eq!(
        made.recurrence.as_ref().unwrap().rules,
        vec![weekly_on_monday()],
        "the rule Stalwart stored is the rule that was sent — through its own reserialization"
    );
    assert_eq!(
        made.start,
        amsterdam("2026-06-01T09:30:00"),
        "a recurring create stays zoned, exactly as a one-off create does"
    );

    let mut base = made.clone();
    base.revisions = created.revisions.clone();
    provider
        .delete_event(account, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}

/// A series ending at a wall clock: the `UNTIL` must reach the server in **UTC** (RFC 5545
/// §3.3.10), because `DTSTART` carries a `TZID`.
///
/// The instant is the caller's to resolve — no adapter carries tzdata — so this also pins
/// the refusal: the same draft without it must not be silently written with a local clock.
pub(crate) async fn an_until_is_written_in_utc(provider: &CalDavProvider, account: &AccountId) {
    let uid = Uid::new("live-caldav-recurring-until@test.local").unwrap();
    pre_clean(provider, account, &uid).await;

    let mut rule = weekly_on_monday();
    rule.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());

    let draft = |recurrence: DraftRecurrence| {
        EventDraft::new(
            provider.calendar_id(),
            uid.clone(),
            "Live recurring UNTIL test",
            amsterdam("2026-06-01T09:30:00"),
            amsterdam("2026-06-01T10:00:00"),
            stamp(),
        )
        .repeating(recurrence)
    };

    // Without the resolved instant the adapter refuses rather than guessing.
    assert!(
        provider
            .create_event(account, &draft(DraftRecurrence::new(rule.clone())))
            .await
            .is_err(),
        "a zoned UNTIL with no resolved instant must be refused, not guessed"
    );

    // 23:59:59 in Europe/Amsterdam is 22:59:59Z on that date (CEST, UTC+2).
    let created = provider
        .create_event(
            account,
            &draft(DraftRecurrence::ending_at(
                rule,
                UtcDateTime::new(2026, 10, 26, 22, 59, 59).unwrap(),
            )),
        )
        .await
        .expect("create a series ending at a wall clock");

    let made = require(provider, account, "live-caldav-recurring-until@test.local").await;
    assert_eq!(
        made.recurrence.as_ref().unwrap().rules[0].bound,
        RecurrenceBound::Until("2026-10-26T22:59:59".parse().unwrap()),
        "the UNTIL round-trips as the UTC instant it was written as — the parser reads the \
         stored clock, so a host wanting the authored 23:59:59 re-resolves through the zone"
    );

    let mut base = made.clone();
    base.revisions = created.revisions.clone();
    provider
        .delete_event(account, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}
