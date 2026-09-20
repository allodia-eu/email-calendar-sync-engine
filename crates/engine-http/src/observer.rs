//! What a host is told when a request is throttled.

use std::time::Duration;

/// One throttled reply, and what the engine decided to do about it.
///
/// **Carries nothing that describes the user's mail.** No URL, no message id, no body — a
/// request path on a mail API names a mailbox or a message, and this exists to be written to a
/// diagnostic log a user attaches to a support request.
#[derive(Debug, Clone, Copy)]
pub struct ThrottleEvent<'a> {
    /// Which adapter was throttled, for the log line: `"gmail"`, `"graph"`, `"jmap"`,
    /// `"caldav"`. A display label, not a discriminant — a host reading it to decide
    /// behaviour is branching on provider kind, which is what the neutral facade exists to
    /// prevent.
    pub provider: &'a str,
    /// The status that triggered the wait — `429`, or `503` on an idempotent request, or
    /// whatever else the adapter's
    /// [`ThrottleClassifier`](crate::ThrottleClassifier) recognised in the body (Gmail's
    /// `403`, JMAP's `400`). It is a status, not a diagnosis: it says what arrived, never
    /// which ceiling was hit.
    pub status: u16,
    /// Which attempt was refused, counting the first send as `0`.
    pub attempt: u32,
    /// How long the engine will wait before sending again, or how long it waited in total
    /// when [`gave_up`](Self::gave_up) is set.
    pub delay: Duration,
    /// The instant the **server itself** named — its `Retry-After`, or a wait an adapter's
    /// classifier read out of the body — or `None` where the backoff schedule chose the delay
    /// unaided.
    ///
    /// Worth logging for two reasons. A server that names a number is describing a real quota
    /// window, and "waiting 120s because the server asked" reads very differently from the
    /// same wait chosen locally. And it is **not** the same number as
    /// [`delay`](Self::delay): while waiting, `delay` is this plus jitter; on a
    /// [`gave_up`](Self::gave_up) event, `delay` is the total already slept — often zero —
    /// and this is the only thing that says when to come back.
    ///
    /// Replaced a `server_asked: bool`, which was exactly `stated.is_some()` and threw the
    /// number away. A log line that can say *how long* the server asked for is the difference
    /// between "still limiting us, the rest waits for the next sync" and knowing when the
    /// next sync will get anywhere.
    ///
    /// The same figure reaches the *caller* as
    /// [`Sent::stated_wait`](crate::Sent::stated_wait), which is what a scope schedules its
    /// next attempt from; this is the copy that goes in the log.
    pub stated: Option<Duration>,
    /// Set on the last event of a request that stayed throttled — the attempts or the total
    /// wait ran out and the `429` is being returned to the caller. Exactly one event per
    /// request carries this.
    pub gave_up: bool,
}

/// A sink the engine notifies when a provider throttles it.
///
/// Implementations must be cheap and non-blocking (record a counter, write a log line); the
/// request awaits nothing on them. The blanket impl over `Fn(&ThrottleEvent)` lets a host pass
/// a closure.
pub trait ThrottleObserver: Send + Sync {
    /// Receives one throttled reply.
    fn throttled(&self, event: &ThrottleEvent<'_>);
}

impl<F: Fn(&ThrottleEvent<'_>) + Send + Sync> ThrottleObserver for F {
    fn throttled(&self, event: &ThrottleEvent<'_>) {
        self(event);
    }
}

/// A [`ThrottleObserver`] that discards every event — the default, for a host that has not
/// wired one up. Backoff still happens; only the reporting is dropped.
#[derive(Debug, Clone, Copy, Default)]
pub struct IgnoreThrottles;

impl ThrottleObserver for IgnoreThrottles {
    fn throttled(&self, _event: &ThrottleEvent<'_>) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{IgnoreThrottles, ThrottleEvent, ThrottleObserver};

    fn event() -> ThrottleEvent<'static> {
        ThrottleEvent {
            provider: "gmail",
            status: 429,
            attempt: 1,
            delay: std::time::Duration::from_millis(750),
            stated: None,
            gave_up: false,
        }
    }

    #[test]
    fn a_closure_is_an_observer() {
        let seen: Mutex<Vec<(u16, u32)>> = Mutex::new(Vec::new());
        let observer = |e: &ThrottleEvent<'_>| seen.lock().unwrap().push((e.status, e.attempt));
        observer.throttled(&event());
        assert_eq!(*seen.lock().unwrap(), vec![(429, 1)]);
    }

    #[test]
    fn the_default_observer_discards() {
        IgnoreThrottles.throttled(&event());
    }
}
