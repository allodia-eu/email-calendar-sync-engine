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

    fn report(
        &self,
        status: u16,
        attempt: u32,
        delay: Duration,
        stated: Option<Duration>,
        gave_up: bool,
    ) {
        self.observer.throttled(&ThrottleEvent {
            provider: self.provider,
            status,
            attempt,
            delay,
            stated,
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
/// the set. A reply a classifier had to read comes back through [`Sent`] with those bytes
/// attached — the reply object itself is untouched, so nothing about it can be lost.
///
/// A reply a classifier claims is replayed **whatever the method was**, on the same reasoning
/// that lets a `429` be: a throttle is a request the server refused rather than performed, so
/// a replay cannot apply it twice. That is a promise the classifier makes — see
/// [`ThrottleClassifier::throttle`](crate::ThrottleClassifier::throttle) — not one the status
/// carries, which is why the `503` rule still asks about idempotency and this does not.
///
/// # Errors
///
/// Returns the `reqwest` error from building or sending the request, or from reading the
/// body of a reply the classifier asked to see.
pub async fn send_retrying(request: RequestBuilder, retry: &RetryConfig) -> reqwest::Result<Sent> {
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
        let mut response = client.execute(pending).await?;
        let status = response.status().as_u16();
        let (body, stated) = if retryable(status, idempotent) {
            // The status settles it, so the body is never touched — which matters beyond
            // efficiency: a JMAP EventSource stream comes back through here, and reading
            // one to the end never returns.
            (None, None)
        } else if retry.classifier.reads_body_of(status) {
            let body = drain(&mut response).await?;
            match retry.classifier.throttle(status, &body) {
                Some(throttle) => (Some(body), throttle.stated_wait()),
                // Exactly the refusal its status says it is — a real `403`, not a quota
                // one. Handed back with the bytes already read, which is what the adapter
                // was about to do itself.
                None => return Ok(Sent::read(response, body)),
            }
        } else {
            return Ok(Sent::whole(response));
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
            retry.report(status, number, waited, retry_after, true);
            return Ok(Sent { response, body });
        };
        retry.report(status, number, wait, retry_after, false);
        tokio::time::sleep(wait).await;
        waited = waited.saturating_add(wait);
        number = number.saturating_add(1);
        pending = next;
    }
}

/// A reply [`send_retrying`] is handing back, and whatever it had to learn on the way.
///
/// # Why this is not just a `reqwest::Response`
///
/// Classifying a refusal means reading its body, and reading a body used to mean losing the
/// reply: `Response::bytes` consumes `self`, so an earlier version of this crate took the
/// reply apart and built a new one around the bytes — copying the status, the version, the
/// headers and the extensions across by hand. That works until it silently does not. The
/// negotiated TLS version lives in an extension, and dropping it would have made a throttled
/// account the one account reporting no TLS version, with nothing failing to say so; the URL
/// could not be carried over at all, because reqwest will not let anything outside itself set
/// one. Every field was one somebody had to remember.
///
/// So nothing is rebuilt. The body is drained through [`Response::chunk`], which takes
/// `&mut self`, and **the reply that comes back is the same object the client produced** —
/// its status, headers, version, extensions and URL are the originals, not copies, so there
/// is no list of fields to keep in step with reqwest.
///
/// What is left is that a drained reply has no body to read a second time, and that is
/// handled by type rather than by care: this derefs to the response for everything taken by
/// reference, while [`text`](Self::text) and [`bytes`](Self::bytes) are inherent methods that
/// hand back the bytes already read. `Response`'s own consuming readers are unreachable
/// through a `Deref`, so a caller cannot reach past this and find an empty body — it does not
/// compile.
pub struct Sent {
    response: Response,
    /// The body, where the funnel had to read it to classify the reply.
    body: Option<Vec<u8>>,
}

impl Sent {
    /// A reply nothing here read — the ordinary case, and the only one a streaming body
    /// survives.
    fn whole(response: Response) -> Self {
        Self {
            response,
            body: None,
        }
    }

    /// A reply whose body a classifier asked to see, and which turned out not to be a
    /// throttle after all.
    fn read(response: Response, body: Vec<u8>) -> Self {
        Self {
            response,
            body: Some(body),
        }
    }

    /// The whole body as bytes, read now or already read here.
    ///
    /// # Errors
    ///
    /// Returns the `reqwest` error from reading the body.
    pub async fn bytes(self) -> reqwest::Result<Vec<u8>> {
        match self.body {
            Some(body) => Ok(body),
            None => Ok(self.response.bytes().await?.to_vec()),
        }
    }

    /// The whole body as text, lossily decoded like `Response::text`.
    ///
    /// # Errors
    ///
    /// Returns the `reqwest` error from reading the body.
    pub async fn text(self) -> reqwest::Result<String> {
        match self.body {
            // `Response::text` decodes by the charset the headers name; these bytes came
            // from a JSON or XML error document, and every provider here serves those as
            // UTF-8. Lossy rather than strict, for the same reason `text` is: a malformed
            // byte in a diagnostic body should not turn into a second failure.
            Some(body) => Ok(String::from_utf8_lossy(&body).into_owned()),
            None => self.response.text().await,
        }
    }

    /// The reply itself, for a caller that wants to stream the body rather than hold it.
    ///
    /// The JMAP EventSource push stream is the one caller: its body never ends, so it must
    /// not be read here. It is a `200`, which no classifier claims, so it always arrives
    /// undrained — and [`bytes`](Self::bytes) would work on it in the sense that it would
    /// never return.
    #[must_use]
    pub fn into_streaming(self) -> Response {
        self.response
    }
}

impl core::ops::Deref for Sent {
    type Target = Response;

    fn deref(&self) -> &Response {
        &self.response
    }
}

impl core::fmt::Debug for Sent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sent")
            .field("status", &self.response.status())
            .field("body_read", &self.body.is_some())
            .finish_non_exhaustive()
    }
}

/// Reads a reply's body to the end without consuming the reply.
///
/// `Response::chunk` takes `&mut self` where `bytes` takes `self`, which is the whole reason
/// this crate no longer reconstructs anything.
async fn drain(response: &mut Response) -> reqwest::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
