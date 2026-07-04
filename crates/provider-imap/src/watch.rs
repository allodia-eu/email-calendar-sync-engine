//! [`ImapWatcher`] — a dedicated standing connection that pushes change
//! notifications for one mailbox via IMAP `IDLE` (RFC 2177).
//!
//! # A separate connection, on purpose
//!
//! A watcher owns its **own** connection, distinct from the [`ImapProvider`](crate::ImapProvider)
//! that syncs the mailbox. A connection in `IDLE` can only send `DONE` — it cannot
//! `FETCH` — so multiplexing watch and sync onto one socket would mean tearing down
//! `IDLE` on every change and racing the connection lock. Keeping them separate lets
//! the watch connection **stay idling continuously** while the host runs a sync on the
//! provider's connection, which is what closes the notification gap: a message arriving
//! while the host syncs the previous one is still seen, because this connection never
//! left `IDLE`. The cost is one extra connection per watched mailbox — the host decides
//! which (and how many) mailboxes warrant it against the server's connection limit,
//! exactly as it owns the bound-mailbox model for sync.
//!
//! # What a watcher does and does not do
//!
//! It translates the `IDLE` wire protocol — the `+ ` continuation, the unsolicited
//! untagged responses, the `DONE` handshake, and the RFC 2177 "re-issue before the
//! server times out" rule — into a clean [`WatchEvent`] stream (`crate::idle` owns the
//! line-level primitives). It does **not** fetch data, schedule, reconnect, or pick a
//! sync strategy: a [`WatchEvent::Changed`] means only "run the scope's sync," and the
//! authoritative reconciliation is that sync (the CONDSTORE/QRESYNC delta). Reconnect
//! policy and the desktop-versus-mobile tradeoffs live in the host (`engine_provider::Watch`).

use std::time::Duration;

use async_trait::async_trait;
use engine_core::ids::MailboxId;
use engine_provider::{ProviderError, ProviderResult, Watch, WatchEvent};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::ImapConfig;
use crate::idle;
use crate::provider::connect_session;
use crate::transport::Connection;

/// The recommended IMAP `IDLE` keep-alive interval (28 minutes) — a margin under
/// RFC 2177's guidance to re-issue `IDLE` at least every 29 minutes so the server does
/// not log an idle connection off. A host passes its own interval to
/// [`ImapWatcher::connect`] (a shorter one detects a dead connection sooner, at the
/// cost of more wake-ups — useful on mobile); this is the default for a desktop watch.
pub const DEFAULT_IDLE_KEEPALIVE: Duration = Duration::from_mins(28);

/// The ceiling a host keep-alive is clamped to, so a too-long interval cannot let the
/// server time the connection out (RFC 2177's 29-minute rule).
const MAX_KEEPALIVE: Duration = Duration::from_mins(28);
/// The floor a host keep-alive is clamped to, so a misconfigured tiny interval cannot
/// busy-loop the re-`IDLE`.
const MIN_KEEPALIVE: Duration = Duration::from_secs(10);

/// Whether the watch connection is currently in `IDLE`, and if so the command tag (to
/// match its eventual `DONE` completion) and the deadline at which it must be
/// re-issued.
enum WatchState {
    NotIdling,
    Idling { tag: String, deadline: Instant },
}

/// A push / change-notification session over a dedicated IMAP connection bound to one
/// mailbox. Implements [`Watch`]; a host drives [`next`](Watch::next) from a task and
/// runs the mailbox's sync on each [`WatchEvent::Changed`] (see this module's docs).
pub struct ImapWatcher<S> {
    conn: Connection<S>,
    mailbox: MailboxId,
    keepalive: Duration,
    state: WatchState,
}

impl<S> core::fmt::Debug for ImapWatcher<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapWatcher")
            .field("mailbox", &self.mailbox)
            .field("keepalive", &self.keepalive)
            .field("idling", &matches!(self.state, WatchState::Idling { .. }))
            .finish_non_exhaustive()
    }
}

impl ImapWatcher<TlsStream<TcpStream>> {
    /// Opens a **dedicated** implicit-TLS connection, logs in, and binds `mailbox` for
    /// a standing `IDLE` watch with the given `keepalive` (see [`DEFAULT_IDLE_KEEPALIVE`];
    /// a host value is clamped to a sane range).
    ///
    /// The `connector` carries the host's trust policy, exactly as for
    /// [`ImapProvider::connect`](crate::ImapProvider::connect) — the library bakes in no
    /// root store. This is a separate connection from the provider's sync session
    /// (see this module's docs).
    ///
    /// # Errors
    ///
    /// A [`ProviderError`] on a TCP/TLS/login failure, a bad server name, or — as
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) —
    /// a server that does not advertise `IDLE` (the host should fall back to polling).
    pub async fn connect(
        config: &ImapConfig,
        connector: TlsConnector,
        mailbox: MailboxId,
        keepalive: Duration,
    ) -> ProviderResult<Self> {
        let conn = connect_session(config, &connector).await?;
        Self::start(conn, mailbox, keepalive).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ImapWatcher<S> {
    /// Builds a watcher over an already-open, logged-in, capability-negotiated
    /// connection bound to `mailbox`: verifies `IDLE` is advertised, then `EXAMINE`s
    /// the mailbox **read-only** (watching never writes and must not reset `\Recent`).
    /// The first [`next`](Watch::next) issues the actual `IDLE`.
    ///
    /// # Errors
    ///
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) if
    /// the server does not advertise `IDLE`, or the classified failure of the `EXAMINE`.
    pub(crate) async fn start(
        mut conn: Connection<S>,
        mailbox: MailboxId,
        keepalive: Duration,
    ) -> ProviderResult<Self> {
        if !conn.idle_advertised() {
            return Err(ProviderError::invalid_state(
                "server does not advertise IMAP IDLE (RFC 2177); fall back to polling",
            ));
        }
        conn.examine(mailbox.as_str()).await?;
        Ok(Self {
            conn,
            mailbox,
            keepalive: keepalive.clamp(MIN_KEEPALIVE, MAX_KEEPALIVE),
            state: WatchState::NotIdling,
        })
    }

    /// Ensures the connection is idling, issuing `IDLE` (and recording its tag +
    /// keep-alive deadline) only when not already, so a watcher **stays** idling across
    /// `Changed` events. Returns the current deadline.
    async fn ensure_idling(&mut self) -> ProviderResult<Instant> {
        match &self.state {
            WatchState::Idling { deadline, .. } => Ok(*deadline),
            WatchState::NotIdling => {
                let tag = idle::idle_start(&mut self.conn).await?;
                let deadline = Instant::now() + self.keepalive;
                self.state = WatchState::Idling { tag, deadline };
                Ok(deadline)
            }
        }
    }

    /// Awaits the next [`WatchEvent`]. The inherent form of [`Watch::next`], so the
    /// loop can be driven without importing the trait.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the connection drops or the server errors
    /// (the host reconnects per its own policy).
    pub async fn next_event(&mut self) -> ProviderResult<WatchEvent> {
        let deadline = self.ensure_idling().await?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, idle::idle_wait_change(&mut self.conn)).await {
            // A change — report it but STAY idling, so a change arriving while the host
            // syncs (on a separate connection) is still captured by the next call.
            Ok(Ok(())) => Ok(WatchEvent::Changed),
            // The connection dropped or the server errored: leave IDLE so a fresh call
            // re-establishes it, and surface the classified failure.
            Ok(Err(err)) => {
                self.state = WatchState::NotIdling;
                Err(ProviderError::from(err))
            }
            // The keep-alive elapsed: end IDLE (the RFC 2177 re-issue), draining any
            // change that landed at the boundary — reported as `Changed`, an otherwise
            // quiet interval as `KeepAlive`. Either way we leave IDLE; the next call
            // re-issues it. The boundary `DONE` doubles as a liveness probe: a silently
            // dead connection fails here and surfaces to the host.
            Err(_elapsed) => {
                let WatchState::Idling { tag, .. } = &self.state else {
                    return Err(ProviderError::invalid_state("watch lost its idle state"));
                };
                let tag = tag.clone();
                let saw_change = idle::idle_done(&mut self.conn, &tag).await?;
                self.state = WatchState::NotIdling;
                Ok(if saw_change {
                    WatchEvent::Changed
                } else {
                    WatchEvent::KeepAlive
                })
            }
        }
    }

    /// Ends the watch gracefully — a `DONE` if currently idling — then drops the
    /// connection. Optional: dropping the watcher without calling this also releases the
    /// connection; `stop` just lets the server see a clean end-of-`IDLE`.
    ///
    /// # Errors
    ///
    /// The classified failure of the final `DONE` (e.g. the connection already dropped).
    pub async fn stop(mut self) -> ProviderResult<()> {
        if let WatchState::Idling { tag, .. } = &self.state {
            let tag = tag.clone();
            idle::idle_done(&mut self.conn, &tag).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> Watch for ImapWatcher<S> {
    async fn next(&mut self) -> ProviderResult<WatchEvent> {
        self.next_event().await
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
