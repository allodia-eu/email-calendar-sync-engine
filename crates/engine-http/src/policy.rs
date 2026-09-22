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
    absorbs_stated: Duration,
}

impl Default for RetryPolicy {
    /// Five sends and at most a minute of waiting, doubling from 500 ms — and **at most five
    /// seconds of it spent on an instant the server named**.
    ///
    /// The last bound is the one that decides where a wait belongs. A backoff the engine
    /// chose is a guess at when a transient condition clears, and absorbing it is what this
    /// crate is for. A number the *server* stated is not a guess: it is the reset of a real
    /// quota window, it can be tens of seconds, and sleeping on it parks a task, holds the
    /// account's gate lane and blocks every sibling provider behind a limit that is not
    /// theirs. Above five seconds it stops being a hiccup and becomes a scheduling decision,
    /// which the north star puts with the host — so the funnel hands it back with the instant
    /// attached rather than sleeping on the host's behalf. See
    /// [`Sent::stated_wait`](crate::Sent::stated_wait).
    fn default() -> Self {
        Self {
            attempts: 5,
            base: Duration::from_millis(500),
            ceiling: Duration::from_secs(30),
            budget: Duration::from_mins(1),
            absorbs_stated: Duration::from_secs(5),
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
            absorbs_stated: Duration::ZERO,
        }
    }
}

/// What one refused attempt came back with, and what has already been spent on it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Attempt {
    /// Whether this reply is one to wait out at all.
    ///
    /// Decided by the caller, because there are now two ways to decide it — [`retryable`]
    /// from the status, or an adapter's
    /// [`ThrottleClassifier`](crate::ThrottleClassifier) from the body — and a policy that
    /// re-derived it from the status would silently overrule the second. That is precisely
    /// the bug this field exists to have fixed: Gmail's quota refusal is a `403`, and a
    /// schedule asked "is 403 retryable?" answers no however carefully the adapter read the
    /// body.
    pub(crate) throttled: bool,
    /// The wait the server named, from its `Retry-After` or from a body the adapter read.
    pub(crate) retry_after: Option<Duration>,
    /// Which send this was, counting the first as `0`.
    pub(crate) number: u32,
    /// Total slept across every earlier wait for this request.
    pub(crate) waited: Duration,
}

impl RetryPolicy {
    /// The wait before sending again, or `None` to hand the reply back as it is.
    ///
    /// `entropy` is any spread of bits; only its low end is used, and only to place the delay
    /// inside its jitter window.
    pub(crate) fn next_delay(&self, attempt: &Attempt, entropy: u64) -> Option<Duration> {
        if !attempt.throttled {
            return None;
        }
        if attempt.number.saturating_add(1) >= self.attempts {
            return None;
        }
        // Never earlier than the server asked: jitter is added above its number, not around
        // it. A little is still needed — a whole wave handed the same `Retry-After` would
        // otherwise return in one block.
        let (floor, spread) = if let Some(asked) = attempt.retry_after {
            // Long enough to be a scheduling decision rather than a hiccup. Handed back so
            // the caller can act on the instant instead of a task sleeping on it.
            if asked > self.absorbs_stated {
                return None;
            }
            (asked, (asked / 4).min(Duration::from_secs(1)))
        } else {
            // Equal jitter: half the backoff always, the other half spread. Full jitter can
            // draw close to zero, which against a limiter that has not reset yet spends an
            // attempt to learn nothing.
            let doubled = self
                .base
                .saturating_mul(1_u32 << attempt.number.min(20))
                .min(self.ceiling);
            (doubled / 2, doubled / 2)
        };
        let delay = floor + scale(spread, entropy);
        if attempt.waited.saturating_add(delay) > self.budget {
            return None;
        }
        Some(delay)
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

    use super::{Attempt, RetryPolicy, retry_after, retryable};

    /// An attempt refused with `status`, classified the way the send funnel classifies one:
    /// by the status rule, on an idempotent request.
    fn refused(status: u16) -> Attempt {
        Attempt {
            throttled: retryable(status, true),
            retry_after: None,
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
        assert!(retryable(429, true));
        assert!(
            retryable(429, false),
            "the request was refused, not applied"
        );
    }

    #[test]
    fn a_503_is_retried_only_where_a_replay_cannot_duplicate_the_request() {
        assert!(retryable(503, true));
        assert!(
            !retryable(503, false),
            "a replayed POST on a 503 is a message sent twice",
        );
    }

    #[test]
    fn a_status_the_rule_declines_is_still_waited_out_once_an_adapter_recognises_it() {
        // Gmail's quota refusal, which is where this whole distinction comes from: the
        // status rule says no, the adapter's classifier says yes, and the schedule follows
        // the adapter. Without this the `403` is handed back and the scope waits a pass.
        let policy = RetryPolicy::default();
        assert!(!retryable(403, true));
        assert!(policy.next_delay(&refused(403), HALFWAY).is_none());
        let classified = Attempt {
            throttled: true,
            ..refused(403)
        };
        assert!(policy.next_delay(&classified, HALFWAY).is_some());
    }

    #[test]
    fn a_wait_an_adapter_read_out_of_a_body_is_the_servers_own_number() {
        // Gmail states the quota window's start in the refusal's `details`, so the wait to
        // its end is the server's word, not a guess — and is treated exactly as a
        // `Retry-After` would be, including by the bound above: absorbed while it is
        // short, handed back once it is a schedule. A window with three seconds left is
        // the first; a fresh one with fifty is the second.
        let policy = RetryPolicy::default();
        let nearly_over = Attempt {
            throttled: true,
            retry_after: Some(Duration::from_secs(3)),
            ..refused(403)
        };
        let wait = policy.next_delay(&nearly_over, 0).expect("classified");
        assert!(wait >= Duration::from_secs(3), "{wait:?}");

        let just_started = Attempt {
            retry_after: Some(Duration::from_secs(50)),
            ..nearly_over
        };
        assert!(policy.next_delay(&just_started, 0).is_none());
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
                    delay >= Duration::from_millis(low) && delay <= Duration::from_millis(high),
                    "attempt {number} with entropy {entropy} gave {delay:?}, \
                     want {low}..={high}ms",
                );
            }
        }
    }

    #[test]
    fn the_same_attempt_under_different_entropy_does_not_wake_at_one_instant() {
        let policy = RetryPolicy::default();
        let attempt = refused(429);
        let first = policy.next_delay(&attempt, 1).expect("retryable");
        let second = policy
            .next_delay(&attempt, u64::MAX - 1)
            .expect("retryable");
        assert_ne!(
            first, second,
            "a wave throttled together must not return together",
        );
    }

    #[test]
    fn the_servers_own_number_wins_and_is_never_undercut() {
        let policy = RetryPolicy::default();
        let attempt = Attempt {
            retry_after: Some(Duration::from_secs(4)),
            ..refused(429)
        };
        let wait = policy.next_delay(&attempt, 0).expect("retryable");
        assert!(
            wait >= Duration::from_secs(4),
            "jitter is added above the server's number, never around it: {wait:?}",
        );
        let jittered = policy.next_delay(&attempt, u64::MAX).expect("retryable");
        assert!(jittered <= Duration::from_secs(5));
    }

    #[test]
    fn a_stated_wait_long_enough_to_be_a_schedule_is_not_absorbed() {
        // The line between a hiccup and a scheduling decision. Below it the funnel sleeps
        // and the caller never knows; above it the caller is handed the instant, because a
        // task asleep for half a minute is holding the account's gate lane and blocking
        // every sibling provider behind a quota that is not theirs.
        let policy = RetryPolicy::default();
        for absorbed in [1_u64, 3, 5] {
            let attempt = Attempt {
                retry_after: Some(Duration::from_secs(absorbed)),
                ..refused(429)
            };
            assert!(
                policy.next_delay(&attempt, HALFWAY).is_some(),
                "{absorbed}s is a hiccup",
            );
        }
        for reported in [6_u64, 11, 30, 59] {
            let attempt = Attempt {
                retry_after: Some(Duration::from_secs(reported)),
                ..refused(429)
            };
            assert!(
                policy.next_delay(&attempt, HALFWAY).is_none(),
                "{reported}s belongs to whoever schedules, not to this task",
            );
        }
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
