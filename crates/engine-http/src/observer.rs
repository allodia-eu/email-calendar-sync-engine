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
    /// The status that triggered the wait — `429`, or `503` on an idempotent request.
    pub status: u16,
    /// Which attempt was refused, counting the first send as `0`.
    pub attempt: u32,
    /// How long the engine will wait before sending again, or how long it waited in total
    /// when [`gave_up`](Self::gave_up) is set.
    pub delay: Duration,
    /// Whether [`delay`](Self::delay) came from the server's own `Retry-After` rather than
    /// from the backoff schedule. Worth logging: a server that names a number is describing a
    /// real quota window, and "waiting 120s because the server asked" reads very differently
    /// from the same wait chosen locally.
    pub server_asked: bool,
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
            server_asked: false,
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
