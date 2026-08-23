//! How long to wait, and whether to wait at all — decided without a clock, a socket or a
//! runtime, so the schedule can be asserted directly rather than inferred from timings.

use std::time::{Duration, SystemTime};

/// The bounds the engine puts on waiting out a throttle.
///
/// Two bounds rather than one, because they stop different things. `attempts` stops a server
/// that keeps saying `429` with a short `Retry-After` from being retried forever; `budget`
/// stops a *single* long `Retry-After` from parking a task for minutes. A pass that gives up
/// is not a failure the user has to act on — the next sync covers the same ground — whereas a
/// task asleep for two minutes is indistinguishable from a hang.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    attempts: u32,
    base: Duration,
    ceiling: Duration,
    budget: Duration,
}

impl Default for RetryPolicy {
    /// Five sends and at most a minute of waiting, doubling from 500 ms.
    ///
    /// The schedule spans the two shapes of limit these providers impose. A per-second quota
    /// (Gmail's concurrency ceiling) clears within the first doubling or two; a per-window
    /// quota (Graph's requests-per-ten-minutes) does not clear at all on this timescale, and
    /// there the `Retry-After` the server sends is what decides — usually to exceed the budget
    /// and hand the work back to the next pass, which is the right answer.
    fn default() -> Self {
        Self {
            attempts: 5,
            base: Duration::from_millis(500),
            ceiling: Duration::from_secs(30),
            budget: Duration::from_mins(1),
        }
    }
}

impl RetryPolicy {
    /// Never wait: a throttled reply is returned to the caller as it arrived.
    ///
    /// For a caller that would rather report a throttle than absorb it — a foreground action
    /// where a silent multi-second stall is worse than an error the user can see.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            attempts: 1,
            base: Duration::ZERO,
            ceiling: Duration::ZERO,
            budget: Duration::ZERO,
        }
    }
}

/// What one refused attempt came back with, and what has already been spent on it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Attempt {
    /// The reply's status code.
    pub(crate) status: u16,
    /// The server's `Retry-After`, where it sent a parseable one.
    pub(crate) retry_after: Option<Duration>,
    /// Whether replaying this request cannot apply it twice.
    pub(crate) idempotent: bool,
    /// Which send this was, counting the first as `0`.
    pub(crate) number: u32,
    /// Total slept across every earlier wait for this request.
    pub(crate) waited: Duration,
}

/// A wait the policy granted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Wait {
    pub(crate) delay: Duration,
    pub(crate) server_asked: bool,
}

impl RetryPolicy {
    /// The wait before sending again, or `None` to hand the reply back as it is.
    ///
    /// `entropy` is any spread of bits; only its low end is used, and only to place the delay
    /// inside its jitter window.
    pub(crate) fn next_delay(&self, attempt: &Attempt, entropy: u64) -> Option<Wait> {
        if !retryable(attempt.status, attempt.idempotent) {
            return None;
        }
        if attempt.number.saturating_add(1) >= self.attempts {
            return None;
        }
        // Never earlier than the server asked: jitter is added above its number, not around
        // it. A little is still needed — a whole wave handed the same `Retry-After` would
        // otherwise return in one block.
        let (floor, spread, server_asked) = if let Some(asked) = attempt.retry_after {
            (asked, (asked / 4).min(Duration::from_secs(1)), true)
        } else {
            // Equal jitter: half the backoff always, the other half spread. Full jitter can
            // draw close to zero, which against a limiter that has not reset yet spends an
            // attempt to learn nothing.
            let doubled = self
                .base
                .saturating_mul(1_u32 << attempt.number.min(20))
                .min(self.ceiling);
            (doubled / 2, doubled / 2, false)
        };
        let delay = floor + scale(spread, entropy);
        if attempt.waited.saturating_add(delay) > self.budget {
            return None;
        }
        Some(Wait {
            delay,
            server_asked,
        })
    }
}

/// Whether this reply is one the engine waits out.
///
/// `429` means the request was refused rather than performed, so replaying it cannot duplicate
/// anything and the method does not matter. `503` carries no such promise — the server may
/// have applied the request and failed on the way back — so it is retried only where a replay
/// is harmless anyway. Note that `Method::is_idempotent` answers `false` for the WebDAV
/// extension methods, which is conservative in the safe direction: `PROPFIND` and `REPORT` are
/// idempotent by their own RFCs and simply will not be retried on a `503`.
pub(crate) fn retryable(status: u16, idempotent: bool) -> bool {
    status == 429 || (status == 503 && idempotent)
}

/// Places `entropy` inside `0..=spread`.
fn scale(spread: Duration, entropy: u64) -> Duration {
    let nanos = u64::try_from(spread.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(entropy % nanos.saturating_add(1))
}

/// Parses a `Retry-After` value (RFC 9110 §10.2.3) into how long to wait from `now`.
///
/// Both forms are read. Delta-seconds is what these services actually send; the HTTP-date form
/// is equally legal, and RFC 9110 §5.6.7 requires a recipient to accept **all three** date
/// syntaxes — IMF-fixdate and the two obsolete ones — which is why this defers to `httpdate`
/// rather than matching the preferred one.
///
/// **A date is honoured only if it is strictly in the future.** That is a real guard, not a
/// formality: an HTTP-date names an absolute instant and is always GMT, so there is no zone to
/// get wrong, but the delay it implies is measured against *this device's* clock rather than
/// the server's. A device an hour slow would otherwise read "available at 07:28 GMT" as an
/// hour of sleeping. `duration_since` fails on a past instant, which is exactly that check.
///
/// A date far in the *future* needs no separate guard: it becomes a large delay, and the
/// policy's total-wait budget declines it the same way it declines a large delta-seconds.
///
/// A zero wait is treated as no answer at all — from either form, so `Retry-After: 0` and a
/// date of exactly now behave alike. Having just been refused, retrying in the same instant
/// spends an attempt to learn nothing; the backoff schedule's quarter-second is the better
/// reading of "now". Anything unparseable falls through to that schedule too.
pub(crate) fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    let asked = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .ok()?
    };
    (!asked.is_zero()).then_some(asked)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Attempt, RetryPolicy, retry_after};

    fn refused(status: u16) -> Attempt {
        Attempt {
            status,
            retry_after: None,
            idempotent: true,
            number: 0,
            waited: Duration::ZERO,
        }
    }

    /// The midpoint of a jitter window, so a schedule assertion is not also asserting a
    /// particular random draw.
    const HALFWAY: u64 = u64::MAX / 2;

    #[test]
    fn a_success_is_never_retried() {
        let policy = RetryPolicy::default();
        for status in [200_u16, 204, 302, 400, 401, 403, 404, 500, 502] {
            assert!(
                policy.next_delay(&refused(status), HALFWAY).is_none(),
                "{status} is not a throttle",
            );
        }
    }

    #[test]
    fn a_429_is_retried_whatever_the_method() {
        let policy = RetryPolicy::default();
        let non_idempotent = Attempt {
            idempotent: false,
            ..refused(429)
        };
        assert!(policy.next_delay(&non_idempotent, HALFWAY).is_some());
    }

    #[test]
    fn a_503_is_retried_only_where_a_replay_cannot_duplicate_the_request() {
        let policy = RetryPolicy::default();
        assert!(policy.next_delay(&refused(503), HALFWAY).is_some());
        let post = Attempt {
            idempotent: false,
            ..refused(503)
        };
        assert!(
            policy.next_delay(&post, HALFWAY).is_none(),
            "a replayed POST on a 503 is a message sent twice",
        );
    }

    #[test]
    fn the_backoff_doubles_and_stays_inside_its_jitter_window() {
        let policy = RetryPolicy::default();
        // base 500ms doubling, equal jitter: the window is [half, whole].
        for (number, low, high) in [(0_u32, 250_u64, 500_u64), (1, 500, 1000), (2, 1000, 2000)] {
            let attempt = Attempt {
                number,
                ..refused(429)
            };
            for entropy in [0, HALFWAY, u64::MAX] {
                let delay = policy.next_delay(&attempt, entropy).expect("retryable");
                assert!(
                    delay.delay >= Duration::from_millis(low)
                        && delay.delay <= Duration::from_millis(high),
                    "attempt {number} with entropy {entropy} gave {:?}, want {low}..={high}ms",
                    delay.delay,
                );
            }
        }
    }

    #[test]
    fn the_same_attempt_under_different_entropy_does_not_wake_at_one_instant() {
        let policy = RetryPolicy::default();
        let attempt = refused(429);
        let first = policy.next_delay(&attempt, 1).expect("retryable").delay;
        let second = policy
            .next_delay(&attempt, u64::MAX - 1)
            .expect("retryable")
            .delay;
        assert_ne!(
            first, second,
            "a wave throttled together must not return together",
        );
    }

    #[test]
    fn the_servers_own_number_wins_and_is_never_undercut() {
        let policy = RetryPolicy::default();
        let attempt = Attempt {
            retry_after: Some(Duration::from_secs(8)),
            ..refused(429)
        };
        let wait = policy.next_delay(&attempt, 0).expect("retryable");
        assert!(wait.server_asked);
        assert!(
            wait.delay >= Duration::from_secs(8),
            "jitter is added above the server's number, never around it: {:?}",
            wait.delay,
        );
        let jittered = policy.next_delay(&attempt, u64::MAX).expect("retryable");
        assert!(jittered.delay <= Duration::from_secs(9));
    }

    #[test]
    fn attempts_run_out() {
        let policy = RetryPolicy::default();
        // Five sends: numbers 0..=3 may be retried, the fifth (number 4) is the last.
        for number in 0..4 {
            let attempt = Attempt {
                number,
                ..refused(429)
            };
            assert!(policy.next_delay(&attempt, HALFWAY).is_some(), "{number}");
        }
        let last = Attempt {
            number: 4,
            ..refused(429)
        };
        assert!(policy.next_delay(&last, HALFWAY).is_none());
    }

    #[test]
    fn a_wait_that_would_overrun_the_budget_is_declined_rather_than_truncated() {
        let policy = RetryPolicy::default();
        // A window-scale Retry-After: the work goes back to the next pass rather than
        // parking this task, which a user cannot tell apart from a hang.
        let long = Attempt {
            retry_after: Some(Duration::from_mins(5)),
            ..refused(429)
        };
        assert!(policy.next_delay(&long, HALFWAY).is_none());

        // And the budget counts what earlier waits already spent.
        let spent = Attempt {
            retry_after: Some(Duration::from_secs(40)),
            waited: Duration::from_secs(30),
            ..refused(429)
        };
        assert!(policy.next_delay(&spent, HALFWAY).is_none());
    }

    #[test]
    fn the_none_policy_waits_for_nothing() {
        assert!(
            RetryPolicy::none()
                .next_delay(&refused(429), HALFWAY)
                .is_none()
        );
    }

    /// `Wed, 21 Oct 2015 07:28:00 GMT` — RFC 9110's own example — as seconds since the epoch.
    const EXAMPLE: u64 = 1_445_412_480;

    fn at(epoch_secs: u64) -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs)
    }

    #[test]
    fn retry_after_reads_delta_seconds() {
        let now = at(EXAMPLE);
        assert_eq!(retry_after("30", now), Some(Duration::from_secs(30)));
        assert_eq!(retry_after("  7 ", now), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_reads_all_three_date_syntaxes() {
        // RFC 9110 §5.6.7 requires a recipient to accept every one of these, not only the
        // preferred form, so each is exercised rather than assumed.
        let now = at(EXAMPLE - 90);
        for value in [
            "Wed, 21 Oct 2015 07:28:00 GMT", // IMF-fixdate, the preferred form
            "Wednesday, 21-Oct-15 07:28:00 GMT", // obsolete RFC 850
            "Wed Oct 21 07:28:00 2015",      // obsolete asctime
        ] {
            assert_eq!(
                retry_after(value, now),
                Some(Duration::from_secs(90)),
                "{value}",
            );
        }
    }

    #[test]
    fn a_date_in_the_past_is_refused_rather_than_read_as_no_wait() {
        // The delay is measured against *this device's* clock, so a skewed one is the
        // failure mode. Backwards is the dangerous direction only because it silently
        // becomes "retry now"; falling through to the backoff schedule is the safe read.
        let value = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(retry_after(value, at(EXAMPLE + 1)), None, "one second past");
        assert_eq!(retry_after(value, at(EXAMPLE + 86_400)), None, "a day past");
        assert_eq!(retry_after(value, at(EXAMPLE)), None, "exactly now");
    }

    #[test]
    fn a_date_far_ahead_is_left_for_the_budget_to_decline() {
        // Not rejected here — a genuine long quota window and a device clock stuck in the
        // past look identical, and both want the same answer: hand the work to the next
        // pass rather than park this one.
        let asked = retry_after("Wed, 21 Oct 2015 07:28:00 GMT", at(EXAMPLE - 86_400));
        assert_eq!(asked, Some(Duration::from_hours(24)));
        let attempt = Attempt {
            retry_after: asked,
            ..refused(429)
        };
        assert!(
            RetryPolicy::default()
                .next_delay(&attempt, HALFWAY)
                .is_none()
        );
    }

    #[test]
    fn a_zero_wait_from_either_form_falls_back_to_the_backoff_schedule() {
        assert_eq!(retry_after("0", at(EXAMPLE)), None);
        assert_eq!(
            retry_after("Wed, 21 Oct 2015 07:28:00 GMT", at(EXAMPLE)),
            None
        );
    }

    #[test]
    fn an_unparseable_value_is_no_answer() {
        let now = at(EXAMPLE);
        for value in ["", "-5", "soon", "Wed, 32 Xxx 2015 07:28:00 GMT", "1e3"] {
            assert_eq!(retry_after(value, now), None, "{value}");
        }
    }
}
