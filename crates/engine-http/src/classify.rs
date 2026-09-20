//! What an adapter recognises in a refusal whose status does not admit to being one.

use core::time::Duration;

/// A refusal an adapter recognised, and the wait the server named for it if it named one.
///
/// Returned by a [`ThrottleClassifier`] for a reply [`retryable`](crate::RetryPolicy) would
/// have handed straight back. It carries no status: the status is what failed to classify the
/// reply, which is the only reason this type exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Throttle {
    after: Option<Duration>,
}

impl Throttle {
    /// A refusal to wait out on the engine's own backoff schedule.
    ///
    /// The right answer whenever the body names no instant, and the honest one whenever it
    /// names something the adapter does not fully trust.
    #[must_use]
    pub const fn new() -> Self {
        Self { after: None }
    }

    /// A refusal the server said would clear after `after`.
    ///
    /// Treated exactly as a `Retry-After` header is — as a floor the backoff is never allowed
    /// to undercut, and as a number the policy's total-wait budget may still decline. Use it
    /// only for an instant the *server* stated. A delay the adapter guessed is a guess the
    /// backoff schedule already makes, and dressing one up as the server's word means a host's
    /// log says `server_asked` about a number no server sent.
    #[must_use]
    pub const fn after(after: Duration) -> Self {
        Self { after: Some(after) }
    }

    /// The server's own wait, where the body named one.
    #[must_use]
    pub const fn stated_wait(&self) -> Option<Duration> {
        self.after
    }
}

/// Reads an adapter's refusals out of the replies whose status does not classify them.
///
/// # Why this is the adapter's job and not this crate's
///
/// `engine-http` exists so four transports share one answer to "how long do I wait". Sharing
/// that does not require sharing *how each server says no*, and the two are easy to confuse:
/// the shortest path to waiting out a Gmail `403` is a `serde_json` call in
/// [`send_retrying`](crate::send_retrying) that looks for `rateLimitExceeded`. That path ends
/// with four providers' error vocabularies in one `match` in the neutral crate — the shape
/// `AGENTS.md` calls "five different answers to the same question", and the thing
/// `http-throttling.md` had already ruled out in as many words: *"sniffing a body to decide
/// would put provider knowledge in the shared layer."*
///
/// Each adapter already classifies these bodies correctly for its own error type, and has
/// done all along. What was missing was a way for that verdict to reach the retry decision,
/// which had already read the status and moved on. This is that way, and it is deliberately
/// the only one: the shared layer still owns every question about *waiting*, and learns
/// nothing about anybody's JSON.
///
/// # What it costs, and why it asks twice
///
/// Classifying a body means reading it, and reading it means buffering it before
/// `send_retrying` knows whether it will hand the reply back. So an adapter is asked
/// [`reads_body_of`](Self::reads_body_of) first, with nothing but the status, and a reply it
/// does not claim is never buffered at all. Say yes only for the statuses that genuinely
/// need it: `403` for Google, `400` for JMAP. Saying yes to `200` would buffer every page
/// the adapter ever fetches.
///
/// A reply the status alone already classifies — a `429`, or a `503` on an idempotent
/// request — is waited out before this is consulted, and is never buffered. This is
/// **additive**: it can turn a reply that was being handed back into one that is waited out,
/// and it can never do the reverse.
pub trait ThrottleClassifier: Send + Sync {
    /// Whether a reply with this status is one whose body decides the question.
    ///
    /// Asked before the body is read, and asked on every non-2xx reply, so it must be cheap
    /// and must answer `false` for everything the status already settles.
    fn reads_body_of(&self, status: u16) -> bool;

    /// What that body turned out to be: `None` for a refusal that is exactly what its status
    /// says — a real `403 insufficientPermissions` is not a throttle and retrying it wastes
    /// a quota unit to be told the same thing.
    ///
    /// `body` is the whole reply, unparsed. It may be empty, and it may not be JSON: a proxy
    /// between the engine and the provider can answer in HTML with any status it likes, so an
    /// implementation parses defensively and answers `None` on anything it does not
    /// recognise.
    fn throttle(&self, status: u16, body: &[u8]) -> Option<Throttle>;
}

/// The classifier a [`RetryConfig`](crate::RetryConfig) has until an adapter supplies one:
/// the status is the whole story, which is exactly what `send_retrying` did before
/// classification existed.
///
/// Correct for `provider-graph` and `provider-caldav`, whose servers were measured saying
/// `429` and nothing else (`docs/agent-guidance/http-throttling.md`) — so they are on this
/// not by omission but because it is the accurate description of what they meet.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusAlone;

impl ThrottleClassifier for StatusAlone {
    fn reads_body_of(&self, _status: u16) -> bool {
        false
    }

    fn throttle(&self, _status: u16, _body: &[u8]) -> Option<Throttle> {
        None
    }
}

/// A closure is a classifier for the statuses it is paired with, so a test — or an adapter
/// whose rule really is one line — needs no named type.
impl<F: Fn(u16, &[u8]) -> Option<Throttle> + Send + Sync> ThrottleClassifier
    for (&'static [u16], F)
{
    fn reads_body_of(&self, status: u16) -> bool {
        self.0.contains(&status)
    }

    fn throttle(&self, status: u16, body: &[u8]) -> Option<Throttle> {
        (self.1)(status, body)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{StatusAlone, Throttle, ThrottleClassifier};

    #[test]
    fn a_throttle_states_a_wait_only_when_the_server_did() {
        assert_eq!(Throttle::new().stated_wait(), None);
        assert_eq!(
            Throttle::after(Duration::from_secs(11)).stated_wait(),
            Some(Duration::from_secs(11)),
        );
    }

    #[test]
    fn the_default_classifier_reads_nothing_and_finds_nothing() {
        // The pre-classification behaviour, kept as a type so an adapter that meets only
        // `429`s says so rather than leaving a field unset.
        for status in [200_u16, 400, 403, 429, 503] {
            assert!(!StatusAlone.reads_body_of(status), "{status}");
            assert_eq!(StatusAlone.throttle(status, b"{}"), None, "{status}");
        }
    }

    #[test]
    fn a_closure_paired_with_its_statuses_is_a_classifier() {
        const READS: &[u16] = &[403];
        let classifier = (READS, |_status: u16, body: &[u8]| {
            body.starts_with(b"slow").then(Throttle::new)
        });
        assert!(classifier.reads_body_of(403));
        assert!(!classifier.reads_body_of(400));
        assert_eq!(
            classifier.throttle(403, b"slow down"),
            Some(Throttle::new())
        );
        assert_eq!(classifier.throttle(403, b"go away"), None);
    }
}
