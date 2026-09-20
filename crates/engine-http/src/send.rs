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
    classify::{StatusAlone, ThrottleClassifier},
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
    classifier: Arc<dyn ThrottleClassifier>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            policy: RetryPolicy::default(),
            observer: Arc::new(IgnoreThrottles),
            provider: "http",
            gate: RequestGate::new(),
            classifier: Arc::new(StatusAlone),
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

    /// Lets `classifier` recognise this adapter's refusals in replies whose status does not
    /// classify them — Gmail's `403` quota refusal, JMAP's `400 maxConcurrentRequests`.
    ///
    /// Called by the adapter, not the host, for the same reason
    /// [`labelled`](Self::labelled) is: how a server says no is the one thing only the
    /// adapter knows. Without one, a status is the whole decision, which is the accurate
    /// description of what Graph and CalDAV meet.
    #[must_use]
    pub fn classifying(mut self, classifier: Arc<dyn ThrottleClassifier>) -> Self {
        self.classifier = classifier;
        self
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
/// # What counts as throttled
///
/// A `429`, or a `503` on an idempotent request — and, where the adapter supplied a
/// [`ThrottleClassifier`], whatever else that classifier recognises in a body. The status
/// rule is applied first and on its own, so the replies that were already waited out are
/// waited out on exactly the path they always were, and a classifier can only ever add to
/// the set. A reply a classifier had to read is reassembled around the bytes it read —
/// status, version, headers and extensions all cross over, and the transfer headers are
/// restated for a body reqwest has already decoded.
///
/// # Errors
///
/// Returns the `reqwest` error from building or sending the request, or from reading the
/// body of a reply the classifier asked to see.
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
        let (response, stated) = if retryable(status, idempotent) {
            // The status settles it: nothing is read, and the reply is the one that
            // arrived rather than one put back together.
            (response, None)
        } else if retry.classifier.reads_body_of(status) {
            let (response, body) = buffered(response).await?;
            match retry.classifier.throttle(status, &body) {
                Some(throttle) => (response, throttle.stated_wait()),
                // Exactly the refusal its status says it is — a real `403`, not a quota
                // one. Handed back read, which is what the adapter was about to do.
                None => return Ok(response),
            }
        } else {
            return Ok(response);
        };
        let retry_after = stated.or_else(|| {
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                // The device clock, which is only ever consulted for the HTTP-date form —
                // and only to reject a date that has already passed.
                .and_then(|value| retry_after(value, SystemTime::now()))
        });
        let granted = retry.policy.next_delay(
            &Attempt {
                throttled: true,
                retry_after,
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

/// Reads a reply's body so a classifier can see it, and hands back a reply that still has
/// one.
///
/// Reading consumes the response, and the adapter is still owed one — so the reply is taken
/// apart and put back together around the bytes just read. Status, HTTP version, headers and
/// extensions all cross over, which is what the adapter and [`ObservedConnection`] actually
/// read from a reply: the negotiated TLS version lives in an extension, and losing it here
/// would make a throttled account the one account that reports no TLS version.
///
/// Two things do not survive, both deliberately:
///
/// - **The URL**, which reqwest will not let anything outside itself set. No adapter reads one, and
///   this is the reason to keep it that way — a request path names the user's own mail, which is
///   also why [`ThrottleEvent`] refuses to carry one.
/// - **The transfer headers.** `Content-Encoding`, `Content-Length` and `Transfer-Encoding`
///   describe the bytes that were on the wire, and reqwest has already decompressed them (Gmail's
///   refusals arrive gzipped). Carrying `Content-Encoding: gzip` over to a body that is no longer
///   gzipped states something false; `Content-Length` is restated, since after decoding it is known
///   exactly.
///
/// [`ObservedConnection`]: crate::ObservedConnection
/// [`ThrottleEvent`]: crate::ThrottleEvent
async fn buffered(response: Response) -> reqwest::Result<(Response, Vec<u8>)> {
    use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, TRANSFER_ENCODING};

    let status = response.status();
    let version = response.version();
    let mut headers = response.headers().clone();
    let extensions = response.extensions().clone();
    let body = response.bytes().await?.to_vec();

    headers.remove(CONTENT_ENCODING);
    headers.remove(TRANSFER_ENCODING);
    headers.insert(CONTENT_LENGTH, body.len().into());

    let mut rebuilt = http::Response::new(body.clone());
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    *rebuilt.extensions_mut() = extensions;
    Ok((Response::from(rebuilt), body))
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
