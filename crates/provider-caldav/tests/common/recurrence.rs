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
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceOverride, RecurrenceRule, Weekday},
    ids::{AccountId, Uid},
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
    Provider,
};
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
        .delete_event(account, None, &EventDeletion::of(&base))
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
        .delete_event(account, None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}

/// Changing the rule keeps the user's per-occurrence work; removing it takes that work
/// with it and leaves one ordinary event.
///
/// Both halves need a real server: the patcher's output is only a claim until something
/// stores it and hands it back, and the second half is the one where a leftover override
/// would keep drawing occurrences under "does not repeat".
pub(crate) async fn a_rule_can_be_changed_and_removed(
    provider: &CalDavProvider,
    account: &AccountId,
) {
    const UID: &str = "live-caldav-rule-edit@test.local";
    let uid = Uid::new(UID).unwrap();
    pre_clean(provider, account, &uid).await;

    let created = provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid.clone(),
                "Live rule edit",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(weekly_on_monday())),
        )
        .await
        .expect("create a recurring event");

    // ---- Change the rule: every Wednesday instead of every Monday. ----
    let mut wednesdays = RecurrenceRule::new(Frequency::Weekly);
    wednesdays.by_day = vec![NDay {
        day: Weekday::We,
        nth_of_period: None,
    }];
    let mut base = require(provider, account, UID).await;
    base.revisions = created.revisions.clone();
    let changed = provider
        .patch_event(
            account,
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).recurrence(DraftRecurrence::new(wednesdays.clone())),
            ),
        )
        .await
        .expect("change the rule");

    let after = require(provider, account, UID).await;
    assert_eq!(
        after.recurrence.as_ref().unwrap().rules,
        vec![wednesdays],
        "the server stored the new rule"
    );

    // ---- Remove it: the series becomes one event. ----
    let mut base = after.clone();
    base.revisions = changed.revisions.clone();
    let cleared = provider
        .patch_event(
            account,
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).clear_recurrence(),
            ),
        )
        .await
        .expect("remove the rule");

    let single = require(provider, account, UID).await;
    assert!(
        !single.is_recurring(),
        "the event no longer recurs: {:?}",
        single.recurrence
    );
    assert!(
        single
            .recurrence
            .as_ref()
            .is_none_or(|r| r.overrides.is_empty()),
        "and carries no orphaned override to materialize as an extra occurrence: {:?}",
        single.recurrence
    );
    assert_eq!(single.title, "Live rule edit", "it is still the same event");

    let mut base = single.clone();
    base.revisions = cleared.revisions.clone();
    provider
        .delete_event(account, None, &EventDeletion::of(&base))
        .await
        .expect("delete the probe event");
}

/// Removing **one occurrence**, including one the user had already edited.
///
/// The offline suite proves the document this builds; only a server says whether it accepts
/// it and hands the occurrence back as excluded. The second half is the one that cannot be
/// reasoned about from the bytes: an override whose instant the rule no longer produces is
/// materialized as an *added* occurrence, so a leftover would keep drawing the very
/// occurrence the user deleted — at the time they had moved it to.
pub(crate) async fn an_occurrence_can_be_removed(provider: &CalDavProvider, account: &AccountId) {
    const UID: &str = "live-caldav-occurrence-delete@test.local";
    let uid = Uid::new(UID).unwrap();
    pre_clean(provider, account, &uid).await;

    let created = provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid.clone(),
                "Live occurrence delete",
                amsterdam("2026-06-01T09:30:00"),
                amsterdam("2026-06-01T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(weekly_on_monday())),
        )
        .await
        .expect("create a recurring event");

    // ---- Edit the 8th's occurrence, so the removal has an override to take with it. ----
    let mut base = require(provider, account, UID).await;
    base.revisions = created.revisions.clone();
    let edited = provider
        .patch_event(
            account,
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::starting(amsterdam("2026-06-08T09:30:00"))),
                EventPatch::new(stamp())
                    .summary("Moved to the afternoon")
                    .start(amsterdam("2026-06-08T14:00:00"))
                    .end(amsterdam("2026-06-08T14:30:00")),
            ),
        )
        .await
        .expect("override one occurrence");

    // ---- Remove that occurrence, and a second one nobody had touched. ----
    let mut revisions = edited.revisions.clone();
    for removed in ["2026-06-08T09:30:00", "2026-06-15T09:30:00"] {
        let mut base = require(provider, account, UID).await;
        base.revisions = revisions.clone();
        provider
            .delete_event(
                account,
                Some(&base),
                &EventDeletion::occurrence(
                    &base,
                    Occurrence::starting(amsterdam(removed)),
                    stamp(),
                ),
            )
            .await
            .expect("remove one occurrence");
        // The PUT reports the new ETag through the next read, not through the delete.
        revisions = require(provider, account, UID).await.revisions;
    }

    let after = require(provider, account, UID).await;
    let recurrence = after.recurrence.as_ref().expect("still a series");
    for removed in ["2026-06-08T09:30:00", "2026-06-15T09:30:00"] {
        assert_eq!(
            recurrence.overrides.get(&removed.parse().unwrap()),
            Some(&RecurrenceOverride::Excluded),
            "{removed} is excluded, and the edit that was on it went with it: {:?}",
            recurrence.overrides
        );
    }
    assert_eq!(
        recurrence.rules,
        vec![weekly_on_monday()],
        "the rest of the series is untouched"
    );
    assert_eq!(after.title, "Live occurrence delete");

    let mut base = after.clone();
    base.revisions = revisions;
    provider
        .delete_event(account, Some(&base), &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}
