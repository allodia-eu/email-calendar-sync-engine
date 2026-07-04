//! Push / change notification: the provider-neutral watch contract.
//!
//! A [`Watch`] session is a long-lived stream of [`WatchEvent`]s telling a host
//! that a sync scope **may have changed** and should be re-synced — the engine's
//! abstraction over the IMAP `IDLE` keep-alive (RFC 2177), and the obvious home for
//! a JMAP push channel (RFC 8620 §7) or a Microsoft Graph webhook later.
//!
//! # Push is a latency optimization, never a source of truth
//!
//! A notification carries **no data**: IMAP `IDLE` reports only that the mailbox's
//! message count changed / something was expunged / a flag moved — never *what*, and
//! only *while a connection is actively idling*. So a watch event means exactly one
//! thing: *"run the scope's normal sync."* The authoritative reconciliation is always
//! that sync (for IMAP, the CONDSTORE/QRESYNC delta — one round trip that reconciles
//! new mail, flag changes, and expunges). This keeps push **bulletproof**: a coalesced
//! burst, a spurious wake, a missed notification, or a dropped connection cannot
//! corrupt the store — the next sync makes it correct, because syncing a scope is
//! idempotent. A host that never watches, and only polls, is fully correct; watching
//! only lowers the *latency* of seeing a change.
//!
//! # Scheduling and reconnection are the host's, not the engine's
//!
//! This contract is a **mechanism**, not a policy. *Which* mailboxes a host watches,
//! *whether* an account uses push versus periodic polling, the reconnect/backoff
//! strategy, and the desktop-versus-mobile tradeoffs all live in the host (a watch
//! holds a standing connection, cheap on desktop and costly on a sleeping phone). The
//! engine only turns the IMAP wire protocol — `IDLE`/`DONE`, the 29-minute re-issue
//! rule, untagged-response classification — into the clean event stream below.
//!
//! # The prescribed host loop
//!
//! To close the inherent notification gap (a change that lands while not idling is
//! never delivered), a host should:
//!
//! 1. **Sync once** before trusting the watch (catch anything that changed while not
//!    connected).
//! 2. Loop on [`Watch::next`]: on [`WatchEvent::Changed`] sync the scope; on
//!    [`WatchEvent::KeepAlive`] optionally run a cheap reconcile sync (a backstop for
//!    a silently-missed notification or a half-dead link).
//! 3. On `Err`, apply its reconnect policy and **sync again** before re-watching.
//!
//! The adapter does its part by keeping the watch connection idling *continuously*
//! across events (so a change arriving while the host syscs on another connection is
//! still captured) and re-issuing `IDLE` before the server's idle timeout.

use async_trait::async_trait;

use crate::error::ProviderResult;

/// What a [`Watch`] session reports.
///
/// Both variants mean "the watch is alive"; only the host's response differs. The
/// enum is `#[non_exhaustive]` so a richer signal (e.g. a scope-tagged change once
/// a provider can attribute one) can be added without breaking matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchEvent {
    /// The watched scope changed — new mail arrived, a flag moved, or a message was
    /// expunged. The host responds by **running the scope's normal sync**; the watch
    /// itself carries no detail about what changed (see the module docs).
    Changed,
    /// The keep-alive interval elapsed with no change — the watch re-issued its
    /// underlying `IDLE` (RFC 2177's "re-issue before the server times out" rule) and
    /// is healthy. A host may ignore this, or use it as a **backstop**: a cheap
    /// periodic reconcile sync that catches a notification the connection missed and
    /// confirms the link is still alive.
    KeepAlive,
}

/// A long-lived push / change-notification session for one sync scope.
///
/// Obtained from a concrete adapter (currently only `provider_imap::ImapWatcher`,
/// over a dedicated connection bound to one mailbox); a host drives it from a task
/// and reacts to each [`WatchEvent`] per this module's docs. The session takes
/// `&mut self` and is not shared, so it is `Send` but not `Sync`. There is no
/// `stop` here — dropping the session releases its connection; a graceful shutdown
/// (an explicit `DONE`) is an adapter affordance, not part of the neutral contract.
#[async_trait]
pub trait Watch: Send {
    /// Awaits the next [`WatchEvent`], blocking until the scope changes or the
    /// keep-alive interval elapses (the adapter re-issues `IDLE` transparently).
    ///
    /// Returning [`WatchEvent::Changed`] leaves the session **still watching**, so the
    /// next call resumes the same stream — a host can sync (on a separate connection)
    /// and immediately await the next change without a gap.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`](crate::ProviderError) when the
    /// connection drops or the server errors; the host reconnects per its own policy
    /// (a transport drop is [`FailureClass::Retryable`](engine_core::error::FailureClass::Retryable)).
    /// Prefer not to drop the returned future mid-flight if the session will be
    /// reused; to stop watching, drop the session.
    async fn next(&mut self) -> ProviderResult<WatchEvent>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;
    use engine_core::error::FailureClass;

    /// A scripted watch: yields a `Changed`, then a `KeepAlive`, then a transport
    /// error — proving the contract is implementable and object-safe, and that a host
    /// can drive it behind `dyn Watch` and branch on each event/`Err`.
    struct ScriptedWatch {
        steps: std::vec::IntoIter<ProviderResult<WatchEvent>>,
    }

    #[async_trait]
    impl Watch for ScriptedWatch {
        async fn next(&mut self) -> ProviderResult<WatchEvent> {
            self.steps
                .next()
                .unwrap_or_else(|| Err(ProviderError::retryable("watch exhausted")))
        }
    }

    #[tokio::test]
    async fn watch_is_object_safe_and_streams_events_then_errors() {
        // Hosts hold the session behind dynamic dispatch (the adapter is chosen at
        // runtime), so the trait must be object-safe.
        let mut watch: Box<dyn Watch> = Box::new(ScriptedWatch {
            steps: vec![
                Ok(WatchEvent::Changed),
                Ok(WatchEvent::KeepAlive),
                Err(ProviderError::retryable("connection reset")),
            ]
            .into_iter(),
        });

        assert_eq!(watch.next().await.unwrap(), WatchEvent::Changed);
        assert_eq!(watch.next().await.unwrap(), WatchEvent::KeepAlive);
        // A drop/transport error surfaces as a classified, retryable error — the host
        // reconnects per its own policy.
        let err = watch.next().await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Retryable);

        // The two events are distinct so a host can act differently on each.
        assert_ne!(WatchEvent::Changed, WatchEvent::KeepAlive);
    }
}
