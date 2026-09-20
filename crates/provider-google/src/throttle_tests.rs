//! [`GoogleThrottles`] against the bytes Gmail actually sent.
//!
//! Both refusals here were captured live, minutes apart, from the same throwaway account —
//! the `429` by firing 200 requests at once, the `403` by holding eight in flight until the
//! per-minute quota ran out. Which probe found which is the whole finding: they are
//! different limits, and a sweep that only ever runs eight wide meets exactly one of them.

use core::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_http::{Throttle, ThrottleClassifier};

use super::{GoogleThrottles, classify, quota_window_remaining};
use crate::error::GoogleError;

/// The captured quota refusal: `403`, `PERMISSION_DENIED`, `reason: rateLimitExceeded`.
const QUOTA: &str = include_str!("../tests/fixtures/error/quota_exceeded.json");
/// The **commoner** captured quota refusal: the same `403`, with the same `ErrorInfo`, and
/// no `window_start_time` in it. Measured across 14,710 refusals in three rested bursts,
/// only **32%** carry that field (42%, 23% and 29% by burst) — so this shape is the one a
/// host meets twice as often, and a reader who saw only `QUOTA` would conclude the instant
/// is always there.
const UNTIMED: &str = include_str!("../tests/fixtures/error/quota_exceeded_untimed.json");
/// The captured concurrency refusal: `429`, `RESOURCE_EXHAUSTED`.
const CONCURRENCY: &str = include_str!("../tests/fixtures/error/concurrency_exceeded.json");
/// A real `403`: the account never granted the scope, and no wait will change that.
const FORBIDDEN: &str = include_str!("../tests/fixtures/error/forbidden.json");

/// `window_start_time` from the captured refusal, and the instant Gmail's own `Date` header
/// says it was served — 49 seconds into the window, so 11 seconds of it were left.
const WINDOW_STARTED: u64 = 1_789_912_553;
const SERVED_AT: u64 = 1_789_912_602;

fn at(epoch_secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(epoch_secs)
}

#[test]
fn only_the_403_is_worth_reading_a_body_for() {
    // The cost control: every other status either settles itself or is not a throttle, and
    // claiming one would buffer bodies to learn nothing. `429` is the pointed omission —
    // Gmail sends one, and `engine-http` has always waited it out on the status alone.
    assert!(GoogleThrottles.reads_body_of(403));
    for settled in [200_u16, 400, 404, 410, 412, 429, 500, 503] {
        assert!(!GoogleThrottles.reads_body_of(settled), "{settled}");
    }
}

#[test]
fn the_captured_quota_refusal_is_recognised_as_a_throttle() {
    // The bug in one line: this `403` is Gmail's per-minute quota, and before #218 nothing
    // waited for it — the scope's sync failed and the work went to the next pass.
    assert!(classify(403, QUOTA.as_bytes(), at(SERVED_AT)).is_some());
}

#[test]
fn a_real_permission_failure_is_not_a_throttle() {
    // The same status, the opposite answer. Retrying this spends a quota unit to be told
    // the same thing, which is why the body has to be read rather than the status guessed.
    assert_eq!(classify(403, FORBIDDEN.as_bytes(), at(SERVED_AT)), None);
}

#[test]
fn the_wait_is_the_rest_of_the_quota_window_the_server_named() {
    // Gmail sends no `Retry-After` on either refusal — checked against every header of both
    // captures. It states the window's start instead, so the wait to the window's end is
    // the server's own number and is treated as one.
    assert_eq!(
        quota_window_remaining(QUOTA, at(SERVED_AT)),
        Some(Duration::from_secs(11)),
    );
    assert_eq!(
        classify(403, QUOTA.as_bytes(), at(SERVED_AT)),
        Some(Throttle::after(Duration::from_secs(11))),
        "and reaches the retry decision as the server's word",
    );
}

#[test]
fn a_window_that_has_already_closed_states_no_wait() {
    // The commonest case once any time has passed: the refusal is still a throttle, it just
    // no longer names an instant, so the engine's backoff schedule decides.
    for after in [
        WINDOW_STARTED + 60,
        WINDOW_STARTED + 61,
        WINDOW_STARTED + 6_000,
    ] {
        assert_eq!(quota_window_remaining(QUOTA, at(after)), None, "{after}");
    }
    assert_eq!(
        classify(403, QUOTA.as_bytes(), at(WINDOW_STARTED + 6_000)),
        Some(Throttle::new()),
        "still a throttle, just not a scheduled one",
    );
}

#[test]
fn a_clock_behind_the_servers_is_not_mistaken_for_a_longer_window() {
    // `window_start_time` is an absolute instant read against *this device's* clock, which
    // is the same trap `Retry-After`'s HTTP-date form has. A device an hour slow would
    // otherwise read a window with 11 seconds left as one with an hour left, and the only
    // thing standing between that and an hour of sleeping is the policy's budget. A wait
    // longer than the window is the clock talking, so it is refused here.
    assert_eq!(quota_window_remaining(QUOTA, at(WINDOW_STARTED - 1)), None);
    assert_eq!(
        quota_window_remaining(QUOTA, at(WINDOW_STARTED - 3_600)),
        None
    );
    // The boundary itself is legitimate: refused in the window's first instant, the whole
    // minute really is still to run.
    assert_eq!(
        quota_window_remaining(QUOTA, at(WINDOW_STARTED)),
        Some(Duration::from_mins(1)),
    );
}

#[test]
fn the_concurrency_refusal_is_left_to_the_status_rule() {
    // Gmail's *other* limit, and the one #218 recorded as never happening. It does happen —
    // 200 simultaneous `messages.get`s draw it inside 37 requests. Nothing here has to
    // recognise it, because `engine-http` waits out a `429` whatever sent it; this pins
    // that the body is never even read, so the two limits cannot be conflated later.
    assert!(!GoogleThrottles.reads_body_of(429));
    assert!(CONCURRENCY.contains("Too many concurrent requests for user."));
}

#[test]
fn anything_that_is_not_the_envelope_falls_back_to_the_backoff_schedule() {
    // A proxy between the engine and Google can answer with any status and any bytes it
    // likes, so the reader is defensive: none of these is a throttle, and none of them
    // panics.
    for body in [
        &b""[..],
        b"<html><body>403 Forbidden</body></html>",
        b"{}",
        b"{\"error\":{}}",
        b"\xff\xfe not utf-8",
    ] {
        assert_eq!(classify(403, body, at(SERVED_AT)), None, "{body:?}");
    }
}

#[test]
fn a_quota_window_that_is_not_per_minute_states_no_wait() {
    // Google states the period in the unit. A daily quota names a reset up to 24 hours out,
    // which is not a wait — it is the next pass's problem, and the budget would decline it
    // anyway. Reading it as a per-minute window would be a silent 24-hour sleep request.
    let daily = QUOTA.replace("1/min/{project}/{user}", "1/d/{project}/{user}");
    assert_eq!(quota_window_remaining(&daily, at(SERVED_AT)), None);
    assert_eq!(
        classify(403, daily.as_bytes(), at(SERVED_AT)),
        Some(Throttle::new()),
        "a throttle still, on the engine's own schedule",
    );
}

#[test]
fn the_commoner_refusal_names_no_window_and_is_a_throttle_regardless() {
    // Two thirds of real refusals look like this: the `ErrorInfo` is there, the quota is
    // named, and `window_start_time` simply is not. So the instant is a bonus, never a
    // guarantee, and the only thing that must always hold is the classification.
    assert_eq!(quota_window_remaining(UNTIMED, at(SERVED_AT)), None);
    assert_eq!(
        classify(403, UNTIMED.as_bytes(), at(SERVED_AT)),
        Some(Throttle::new()),
        "a refusal that does not say when is still a refusal",
    );
}

#[test]
fn a_refusal_with_no_details_at_all_is_still_a_throttle() {
    // The older shape, and whatever Google sends next: the `ErrorInfo` is metadata, not a
    // contract, so everything below it is best-effort and its absence changes nothing about
    // whether the reply gets waited out.
    let bare = r#"{"error":{"code":403,"message":"User-rate limit exceeded.","errors":[
        {"message":"User-rate limit exceeded.","domain":"usageLimits",
         "reason":"rateLimitExceeded"}],"status":"PERMISSION_DENIED"}}"#;
    assert_eq!(quota_window_remaining(bare, at(SERVED_AT)), None);
    assert_eq!(
        classify(403, bare.as_bytes(), at(SERVED_AT)),
        Some(Throttle::new()),
    );
}

#[test]
fn a_quota_measured_in_days_is_classified_but_not_waited_out() {
    // Classifying and waiting are different questions. `GoogleError` calls all four of
    // Google's rate-limit reasons `RateLimited`, and should: a host backs off for any of
    // them. But the funnel can only outlast the short ones — retrying a daily quota five
    // times spends five requests to be told the same thing, for a window that clears
    // tomorrow. Before a classifier existed the reply came straight back after one send,
    // and for these two it still does.
    for long in ["dailyLimitExceeded", "quotaExceeded"] {
        let refusal = QUOTA.replace("rateLimitExceeded", long);
        assert_eq!(
            GoogleError::status(403, &refusal).failure_class(),
            engine_core::error::FailureClass::RateLimited,
            "{long} is still a rate limit for the host",
        );
        assert_eq!(
            classify(403, refusal.as_bytes(), at(SERVED_AT)),
            None,
            "{long} names a window this funnel cannot wait out",
        );
    }
}

#[test]
fn the_short_reasons_are_still_waited_out() {
    // The other half, so the narrowing above cannot quietly swallow the case the whole
    // classifier exists for. `rateLimitExceeded` is what all 14,710 measured refusals said.
    for short in ["rateLimitExceeded", "userRateLimitExceeded"] {
        let refusal = QUOTA.replace("rateLimitExceeded", short);
        assert!(
            classify(403, refusal.as_bytes(), at(SERVED_AT)).is_some(),
            "{short}",
        );
    }
}

#[test]
fn the_instant_reaches_the_neutral_error_a_host_and_the_outbox_read() {
    use engine_provider::ProviderError;

    // The whole point of #218's follow-up, end to end through the adapter: the number the
    // server named has to survive as far as `ProviderError::retry_after`, because that is
    // what `engine-store`'s `retry_delay` obeys — outright, past its own 30-minute cap —
    // and what a host schedules a refused scope from. Before this it was computed for a
    // backoff and then dropped on the floor.
    let refused = GoogleError::status(403, QUOTA).with_retry_after(Some(Duration::from_secs(11)));
    assert_eq!(refused.retry_after(), Some(Duration::from_secs(11)));

    let neutral: ProviderError = refused.into();
    assert_eq!(
        neutral.class(),
        engine_core::error::FailureClass::RateLimited
    );
    let stated = neutral
        .retry_after()
        .expect("the instant survives the conversion");
    assert_eq!(stated.seconds(), 11);
    assert_eq!(stated.days(), 0, "an exact span has no nominal day part");
}

#[test]
fn a_failure_that_is_not_a_throttle_names_no_instant_however_it_was_built() {
    use engine_provider::ProviderError;

    // `retry_after` is only ever read alongside `RateLimited`, so a stray instant on
    // another class would be a number a host might act on for a failure that waiting
    // cannot fix. A real `403` is the case that matters, since it shares its status with
    // the quota refusal.
    let forbidden =
        GoogleError::status(403, FORBIDDEN).with_retry_after(Some(Duration::from_secs(11)));
    let neutral: ProviderError = forbidden.into();
    assert_eq!(neutral.class(), engine_core::error::FailureClass::Permanent);
    assert_eq!(neutral.retry_after(), None);
}

#[test]
fn a_throttle_the_server_timed_no_better_than_a_shrug_carries_nothing() {
    use engine_provider::ProviderError;

    // The common JMAP-shaped case, and Gmail's too once the `details` block is absent: a
    // genuine rate limit that named no instant. `None` has to mean "nobody said", not
    // "zero" — the outbox reads it as "fall back to your own backoff".
    let neutral: ProviderError = GoogleError::status(429, CONCURRENCY).into();
    assert_eq!(
        neutral.class(),
        engine_core::error::FailureClass::RateLimited
    );
    assert_eq!(neutral.retry_after(), None);
}
