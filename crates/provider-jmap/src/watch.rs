//! [`JmapWatcher`] — push / change notification over the JMAP **EventSource**
//! channel (RFC 8620 §7.3), the JMAP counterpart of the IMAP `IDLE` watcher.
//!
//! # A latency optimization, never a source of truth
//!
//! Like every [`Watch`], this only tells a host *"a watched scope may have changed —
//! run its normal sync."* A JMAP `StateChange` carries the new per-type state strings
//! but **no object data**, so a [`WatchEvent::Changed`] is a wake hint, not a delta;
//! the authoritative reconciliation is the scope's ordinary `Foo/changes` sync, which
//! is idempotent. A coalesced burst, a spurious wake, or a dropped stream cannot
//! corrupt the store, and a poll-only host is fully correct (`engine_provider::watch`).
//!
//! # What it does
//!
//! It holds a dedicated long-lived `text/event-stream` connection (a separate
//! connection from the sync client, exactly as IMAP watching uses a separate socket),
//! parses the [Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
//! frames the server pushes, and maps them onto the neutral event stream: a `state`
//! event naming a watched JMAP type → [`WatchEvent::Changed`]; a `ping` (the server
//! keep-alive requested via `ping=<secs>`) → [`WatchEvent::KeepAlive`]. A state change
//! for a type the watcher does not track is ignored (it keeps reading). Reconnection
//! and the poll-vs-push policy live in the host, not here.

use std::{collections::VecDeque, time::Duration};

use async_trait::async_trait;
use engine_core::sync::JmapDataType;
use engine_provider::{ProviderError, ProviderResult, Watch, WatchEvent};
use serde_json::Value;

use crate::{JmapClient, JmapConfig, error::JmapError};

/// A sensible default server-`ping` interval for the EventSource keep-alive. A host
/// may pass a shorter one to detect a dead connection sooner (at the cost of more
/// wake-ups), or a longer one to be quieter.
pub const DEFAULT_EVENT_SOURCE_PING: Duration = Duration::from_secs(30);

/// The source of raw event-stream bytes, abstracted so the SSE parsing and event
/// classification are unit-tested offline against scripted chunks while the live
/// watcher reads a real streaming HTTP response.
#[async_trait]
pub(crate) trait ChunkSource: Send {
    /// Reads the next chunk of the stream, or `None` at end of stream (the connection
    /// closed).
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, JmapError>;
}

/// A [`ChunkSource`] backed by a live streaming `reqwest::Response`.
struct ResponseChunks {
    response: reqwest::Response,
}

#[async_trait]
impl ChunkSource for ResponseChunks {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, JmapError> {
        Ok(self.response.chunk().await?.map(|bytes| bytes.to_vec()))
    }
}

/// A push / change-notification session over a JMAP EventSource stream. Implements
/// [`Watch`]; a host drives [`next`](Watch::next) from a task and runs the watched
/// scope's sync on each [`WatchEvent::Changed`] (see `engine_provider::watch`).
///
/// The byte source is boxed rather than a type parameter so the public type stays
/// concrete (the streaming HTTP source is an internal detail; the offline tests swap
/// in a scripted one).
pub struct JmapWatcher {
    source: Box<dyn ChunkSource>,
    parser: SseParser,
    /// Classified events parsed but not yet returned (one chunk can carry several).
    ready: VecDeque<WatchEvent>,
    /// The JMAP type names whose state changes count as [`WatchEvent::Changed`]; empty
    /// means "any type" (paired with a `*` subscription).
    types: Vec<String>,
}

impl core::fmt::Debug for JmapWatcher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JmapWatcher")
            .field("types", &self.types)
            .finish_non_exhaustive()
    }
}

impl JmapWatcher {
    /// Connects a **dedicated** JMAP session and opens its EventSource stream, watching
    /// the given `types` (empty ⇒ every type) with the server pinging every `ping`.
    ///
    /// This is a separate connection from the sync provider, so the stream keeps
    /// receiving while the host syncs on another connection (closing the
    /// notification gap). Mail hosts typically watch
    /// `[JmapDataType::Email, JmapDataType::Mailbox]`.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] on a connect/HTTP failure, or — as
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) —
    /// a server that advertises no `eventSourceUrl` (the host should fall back to
    /// polling).
    pub async fn connect(
        config: JmapConfig,
        types: &[JmapDataType],
        ping: Duration,
    ) -> ProviderResult<Self> {
        let client = JmapClient::connect(config).await.map_err(map_open_error)?;
        let type_names: Vec<&str> = types.iter().map(JmapDataType::as_str).collect();
        let response = client
            .open_event_source(&type_names, ping)
            .await
            .map_err(map_open_error)?;
        Ok(Self::from_source(
            Box::new(ResponseChunks { response }),
            types,
        ))
    }

    /// Builds a watcher over an already-open chunk source watching `types`.
    fn from_source(source: Box<dyn ChunkSource>, types: &[JmapDataType]) -> Self {
        Self {
            source,
            parser: SseParser::default(),
            ready: VecDeque::new(),
            types: types.iter().map(|t| t.as_str().to_owned()).collect(),
        }
    }

    /// Awaits the next mapped [`WatchEvent`], reading and parsing stream chunks until
    /// one produces a `Changed`/`KeepAlive` (skipping state changes for unwatched
    /// types). The inherent form of [`Watch::next`].
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the stream errors or closes (the host
    /// reconnects per its own policy; a closed stream is
    /// [`FailureClass::Retryable`](engine_core::error::FailureClass::Retryable)).
    pub async fn next_event(&mut self) -> ProviderResult<WatchEvent> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(event);
            }
            let Some(bytes) = self
                .source
                .next_chunk()
                .await
                .map_err(ProviderError::from)?
            else {
                return Err(ProviderError::retryable("JMAP event stream closed"));
            };
            for event in self.parser.push(&bytes) {
                if let Some(mapped) = classify(&event, &self.types) {
                    self.ready.push_back(mapped);
                }
            }
        }
    }
}

#[async_trait]
impl Watch for JmapWatcher {
    async fn next(&mut self) -> ProviderResult<WatchEvent> {
        self.next_event().await
    }
}

/// Maps a connect/open [`JmapError`] into a [`ProviderError`], but reports a missing
/// `eventSourceUrl` as `InvalidState` (not-watchable — the host polls) rather than the
/// permanent default, mirroring the IMAP "server does not advertise IDLE" contract.
fn map_open_error(err: JmapError) -> ProviderError {
    match &err {
        JmapError::Session(detail) if detail.contains("eventSourceUrl") => {
            ProviderError::invalid_state(
                "server advertises no JMAP EventSource (RFC 8620 §7.3); fall back to polling",
            )
        }
        _ => ProviderError::from(err),
    }
}

/// Classifies one parsed SSE event against the watched `types`.
///
/// A `state` event → [`WatchEvent::Changed`] when its `StateChange` names a watched
/// type (or `types` is empty ⇒ any change); a `ping` → [`WatchEvent::KeepAlive`];
/// anything else (an unwatched state change, an unknown event) → `None`, so the caller
/// keeps reading.
fn classify(event: &SseEvent, types: &[String]) -> Option<WatchEvent> {
    match event.event.as_str() {
        "state" if state_change_hits(&event.data, types) => Some(WatchEvent::Changed),
        "ping" => Some(WatchEvent::KeepAlive),
        _ => None,
    }
}

/// Whether a `StateChange` payload (RFC 8620 §7.1) reports a change to any watched
/// type. A [`StateChange`](https://www.rfc-editor.org/rfc/rfc8620#section-7.1) is
/// `{ "@type": "StateChange", "changed": { "<accountId>": { "<Type>": "<state>" } } }`.
/// Matching any account is deliberate: the stream is authenticated as one principal, so
/// any watched-type change is worth a (idempotent) sync, and an over-eager wake is
/// harmless while a missed one is not. Empty `types` matches any account that reports at
/// least one changed type — an empty per-account object (`{}`, no type changed) is not a
/// hit, so a bare account entry does not fire a spurious wake.
fn state_change_hits(data: &str, types: &[String]) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    let Some(changed) = value.get("changed").and_then(Value::as_object) else {
        return false;
    };
    changed.values().any(|per_account| {
        per_account.as_object().is_some_and(|map| {
            if types.is_empty() {
                !map.is_empty()
            } else {
                types.iter().any(|t| map.contains_key(t))
            }
        })
    })
}

/// One parsed Server-Sent Event: its `event` field (default `"message"`) and the
/// concatenated `data` payload.
struct SseEvent {
    event: String,
    data: String,
}

/// An incremental Server-Sent Events line parser. Buffers raw bytes (a chunk may split
/// a line, or a multi-byte char at a line's interior) and emits an [`SseEvent`] on each
/// blank-line boundary, per the SSE spec: `event:`/`data:` fields, `data` lines joined
/// with `\n`, comment lines (`:`) ignored, `\r\n`/`\n` line endings.
#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Appends `bytes` and returns every event now complete.
    fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // drop a '\r' from a CRLF ending
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if let Some(event) = self.take_event() {
                    events.push(event);
                }
            } else if !line.starts_with(':') {
                self.consume_field(&line);
            }
            // A comment line (starts with ':') is a keep-alive with no fields; ignore.
        }
        events
    }

    /// Records a `field: value` line (a bare `field` is `field` with an empty value).
    fn consume_field(&mut self, line: &str) {
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {} // id / retry / unknown fields are irrelevant here
        }
    }

    /// Dispatches the buffered event on a blank line, clearing the accumulators. Yields
    /// `None` for an empty record (a blank line with nothing pending).
    fn take_event(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data);
        if event.is_none() && data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event: event.unwrap_or_else(|| "message".to_owned()),
            data: data.join("\n"),
        })
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
