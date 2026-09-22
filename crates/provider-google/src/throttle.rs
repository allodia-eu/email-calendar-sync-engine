//! Which of Gmail's two refusals a status can recognise, and which one it cannot.
//!
//! Gmail says no in two shapes, both captured live on 2026-09-20 and both in
//! `tests/fixtures/error/`:
//!
//! | | Status | `error.status` | Message | What it bounds |
//! |---|---|---|---|---|
//! | `concurrency_exceeded.json` | `429` | `RESOURCE_EXHAUSTED` | "Too many concurrent requests for user." | requests **in flight** |
//! | `quota_exceeded.json` | `403` | `PERMISSION_DENIED` | "Quota exceeded … 'Units per minute per user' …" | requests **per minute** |
//!
//! The `429` needs nothing from here: `engine-http` waits out a `429` whatever sent it. The
//! `403` is the problem, and it is a nasty one, because **a `403` is Google's ordinary
//! not-allowed status**. `error.status` says `PERMISSION_DENIED`; only
//! `errors[0].reason` — `rateLimitExceeded` — separates a quota refusal from a scope the
//! user never granted. Retrying the latter spends a quota unit to be told the same thing, so
//! guessing from the status is wrong in both directions.
//!
//! [`GoogleError`] has read that distinction correctly since the adapter landed. What was
//! missing was any way for its verdict to reach the retry decision, which had already looked
//! at `403`, decided it was nothing to do with throttling, and returned. That is
//! #218; this is the adapter's half of the answer.
//!
//! # The window Gmail names but does not put in a header
//!
//! Neither refusal carries a `Retry-After` — measured, both captures, every header. The
//! `403` does something better: its `google.rpc.ErrorInfo` names
//! `quota_limit: totalQueryCostPerMinutePerUser`, `quota_unit: 1/min/{project}/{user}` and a
//! `window_start_time`. The captured refusal was served 49 seconds after its window began,
//! so the quota had 11 seconds left to run — a number the *server* stated, and one no
//! backoff schedule could have found. Without it the engine's schedule spends all five
//! attempts inside the first eight seconds of a minute-long window and gives up having
//! learned nothing.
//!
//! It is read defensively, because it is response *metadata* and not a documented contract:
//! anything missing, unparseable, not per-minute, already past, or further away than the
//! window is long falls back to the backoff schedule. Every one of those failures lands on
//! the behaviour that existed before this file.

use core::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::error::FailureClass;
use engine_http::{Throttle, ThrottleClassifier};
use serde_json::Value;

use crate::error::GoogleError;

/// The one status whose body decides whether it is a throttle.
///
/// Deliberately not `429`: that one the status settles, and claiming it would buy a buffered
/// body and no new information. Deliberately not `400` either — Google's
/// `FAILED_PRECONDITION` is a stale-etag conflict and its `INVALID_ARGUMENT` is a bad
/// request, and waiting out either is waiting for something that will not change.
const DECIDED_BY_ITS_BODY: &[u16] = &[403];

/// The rate-limit reasons worth *waiting out*, as opposed to merely classifying.
///
/// Google names four, and [`GoogleError`] calls all four `RateLimited` — rightly, because a
/// host should back off for any of them. Only these two describe a limit that clears on a
/// timescale this funnel can outlast. `dailyLimitExceeded` and `quotaExceeded` name a quota
/// measured in days: retrying one five times spends five requests to be told the same thing,
/// and before a classifier existed the reply came straight back after one. So they stay
/// classified and stop being retried, which is the behaviour they had all along.
///
/// The split is by Google's documented meanings rather than by capture — every one of the
/// 14,710 refusals measured here was `rateLimitExceeded`, so there is no observed
/// `dailyLimitExceeded` body to point at. That is the conservative direction: the reasons
/// that stay are the ones measured to be short.
const CLEARS_SOON: &[&str] = &["rateLimitExceeded", "userRateLimitExceeded"];

/// How long a `totalQueryCostPerMinutePerUser` window runs.
///
/// Named by the refusal itself as `1/min/{project}/{user}`; a unit naming any other period
/// is not this window and is not treated as one.
const QUOTA_WINDOW: Duration = Duration::from_mins(1);

/// Reads Gmail's quota refusal out of the `403` that carries it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GoogleThrottles;

impl ThrottleClassifier for GoogleThrottles {
    fn reads_body_of(&self, status: u16) -> bool {
        DECIDED_BY_ITS_BODY.contains(&status)
    }

    fn throttle(&self, status: u16, body: &[u8]) -> Option<Throttle> {
        classify(status, body, SystemTime::now())
    }
}

/// The whole verdict, with the clock passed in so the tests can hold one still — the same
/// shape `engine-http` gives `retry_after`, and for the same reason: the one input here
/// that is not in the response is the device's own clock.
fn classify(status: u16, body: &[u8], now: SystemTime) -> Option<Throttle> {
    let body = core::str::from_utf8(body).ok()?;
    // The adapter's own classification, unchanged and not duplicated: whatever
    // `GoogleError` calls a rate limit is the starting point, so the two can never drift
    // into disagreeing about what a body *is*.
    let refusal = GoogleError::status(status, body);
    if refusal.failure_class() != FailureClass::RateLimited {
        return None;
    }
    // Classifying and waiting are different questions, and this is the one place they part
    // company: a daily quota is every bit as much a rate limit, and waiting for one here
    // would spend five requests on a window measured in days.
    let GoogleError::Status {
        reason: Some(reason),
        ..
    } = &refusal
    else {
        return None;
    };
    if !CLEARS_SOON.contains(&reason.as_str()) {
        return None;
    }
    Some(quota_window_remaining(body, now).map_or_else(Throttle::new, Throttle::after))
}

/// How long the per-minute quota window named in `body` still has to run at `now`.
///
/// `None` whenever the answer would be a guess rather than the server's word — see the
/// module docs for why each of those falls back rather than approximating.
fn quota_window_remaining(body: &str, now: SystemTime) -> Option<Duration> {
    let envelope: Value = serde_json::from_str(body).ok()?;
    let metadata = envelope
        .get("error")?
        .get("details")?
        .as_array()?
        .iter()
        // `details` is a list of typed messages: the quota figures are on the `ErrorInfo`,
        // and a `Help` message sits beside it carrying only a documentation link.
        .find(|detail| {
            detail.get("@type").and_then(Value::as_str)
                == Some("type.googleapis.com/google.rpc.ErrorInfo")
        })?
        .get("metadata")?;

    // Only a per-minute window. A daily quota's unit states a reset up to 24 hours out,
    // which is not a wait — it is the next pass's problem.
    if metadata.get("quota_unit").and_then(Value::as_str)? != "1/min/{project}/{user}" {
        return None;
    }
    let started: u64 = metadata
        .get("window_start_time")
        .and_then(Value::as_str)?
        .parse()
        .ok()?;
    let ends = UNIX_EPOCH
        .checked_add(Duration::from_secs(started))?
        .checked_add(QUOTA_WINDOW)?;
    let remaining = ends.duration_since(now).ok()?;
    // A device clock behind the server's turns a window that is nearly over into one that
    // has barely begun, and a `window_start_time` from an earlier window does the same.
    // Neither can name a wait longer than the window itself, so one that does is the clock
    // talking rather than the server.
    (!remaining.is_zero() && remaining <= QUOTA_WINDOW).then_some(remaining)
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;
