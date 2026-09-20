//! The send funnel every HTTP adapter routes through.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use reqwest::{RequestBuilder, Response, header::RETRY_AFTER};

use crate::{
    gate::RequestGate,
    observer::{IgnoreThrottles, ThrottleEvent, ThrottleObserver},
    policy::{Attempt, RetryPolicy, retry_after, retryable},
};

/// How one adapter answers a throttle: the bounds, where to report, and who to report as.
///
/// A host builds this once and hands it to every provider it configures, the same way it
/// hands out one `TlsClientConfig`; the adapter adds its own
/// label with [`labelled`](Self::labelled) when it builds its transport.
#[derive(Clone)]
pub struct RetryConfig {
    policy: RetryPolicy,
    observer: Arc<dyn ThrottleObserver>,
    provider: &'static str,
    gate: RequestGate,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            policy: RetryPolicy::default(),
            observer: Arc::new(IgnoreThrottles),
            provider: "http",
            gate: RequestGate::new(),
        }
    }
}

impl core::fmt::Debug for RetryConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RetryConfig")
            .field("policy", &self.policy)
            .field("provider", &self.provider)
            .field("gate", &self.gate)
            .finish_non_exhaustive()
    }
}

impl RetryConfig {
    /// Replaces the bounds on waiting.
    #[must_use]
    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sends every throttle to `observer`. Without one the waits still happen; a user just
    /// has nothing in the log explaining the pause.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn ThrottleObserver>) -> Self {
        self.observer = observer;
        self
    }

    /// Names the adapter these requests belong to, for the reported event. Called by the
    /// adapter, not the host: the host's configuration is provider-neutral, and the label is
    /// the one thing only the adapter knows.
    #[must_use]
    pub fn labelled(mut self, provider: &'static str) -> Self {
        self.provider = provider;
        self
    }

    /// Sends these requests through `gate`, so they are counted against the account's
    /// ceiling together with every other provider sharing it.
    ///
    /// **One gate belongs to one account**, and a host builds it: only the host knows where
    /// an account ends. Sharing a config — and therefore a gate — across two accounts bounds
    /// them jointly, which is stricter than any server asked for; the policy and the observer
    /// genuinely are process-wide, so a host that keeps one `RetryConfig` around clones it
    /// per account through this rather than building a second.
    ///
    /// Without one, requests are unbounded here and the ceiling is whatever the caller's own
    /// fan-out happens to be. See [`RequestGate`] for why that is not good enough.
    #[must_use]
    pub fn gated(mut self, gate: RequestGate) -> Self {
        self.gate = gate;
        self
    }

    /// The account ceiling these requests are counted against, for an adapter to
    /// [`narrow_to`](RequestGate::narrow_to) what its own server allows as it connects.
    #[must_use]
    pub fn gate(&self) -> &RequestGate {
        &self.gate
    }

    fn report(&self, status: u16, attempt: u32, delay: Duration, asked: bool, gave_up: bool) {
        self.observer.throttled(&ThrottleEvent {
            provider: self.provider,
            status,
            attempt,
            delay,
            server_asked: asked,
            gave_up,
        });
    }
}

/// Sends `request`, waiting out a throttled reply and sending it again.
///
/// Returns the last response received, throttled or not — this absorbs the *waiting*, never
/// the outcome, so an adapter's own status handling is unchanged and a request that stays
/// throttled still surfaces as the rate limit it is.
///
/// A lane of the account's [`RequestGate`] is held for the whole call, **the backoff sleeps
/// included**: a request waiting out a `Retry-After` is still a request in progress, and
/// letting another one take its lane while the server is asking for less is how one throttle
/// becomes several. Where no gate was wired, nothing is bounded here.
///
/// A transport failure (connection reset, timeout) is returned immediately rather than
/// retried: whether the server acted on the request is unknowable from here, and the sync
/// pass above already treats a failed pass as one to repeat.
///
/// # Errors
///
/// Returns the `reqwest` error from building or sending the request.
pub async fn send_retrying(
    request: RequestBuilder,
    retry: &RetryConfig,
) -> reqwest::Result<Response> {
    let (client, built) = request.build_split();
    let mut pending = built?;
    // Held until this returns, so the account's ceiling counts a retrying request once,
    // from its first attempt to its last.
    let _lane = retry.gate.acquire().await;
    let idempotent = pending.method().is_idempotent();
    let mut number: u32 = 0;
    let mut waited = Duration::ZERO;
    loop {
        // Cloned before the send, which consumes it. `None` for a body reqwest cannot
        // replay (a stream); no adapter here sends one, and the arm below is what happens
        // if one ever does.
        let replay = pending.try_clone();
        let response = client.execute(pending).await?;
        let status = response.status().as_u16();
        if !retryable(status, idempotent) {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            // The device clock, which is only ever consulted for the HTTP-date form — and
            // only to reject a date that has already passed.
            .and_then(|value| retry_after(value, SystemTime::now()));
        let granted = retry.policy.next_delay(
            &Attempt {
                status,
                retry_after,
                idempotent,
                number,
                waited,
            },
            entropy(),
        );
        let (Some(wait), Some(next)) = (granted, replay) else {
            retry.report(status, number, waited, retry_after.is_some(), true);
            return Ok(response);
        };
        retry.report(status, number, wait.delay, wait.server_asked, false);
        tokio::time::sleep(wait.delay).await;
        waited = waited.saturating_add(wait.delay);
        number = number.saturating_add(1);
        pending = next;
    }
}

/// Bits to place one backoff inside its jitter window.
///
/// Not a random number generator and not trying to be one: the only property that matters is
/// that requests throttled in the same instant do not wake in the same instant, and a counter
/// through a mixing function gives that. It is deliberately unseeded, so a run is reproducible
/// — there is nothing here for entropy to protect.
fn entropy() -> u64 {
    /// SplitMix64's increment, which is also its mixing constant.
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    static STATE: AtomicU64 = AtomicU64::new(GAMMA);
    let mut z = STATE.fetch_add(GAMMA, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod entropy_tests {
    use super::entropy;

    #[test]
    fn successive_draws_differ() {
        let draws: std::collections::HashSet<u64> = (0..64).map(|_| entropy()).collect();
        assert_eq!(
            draws.len(),
            64,
            "a repeat would put two waves on one instant"
        );
    }
}
