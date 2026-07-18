//! `provider-google` — the Google (Gmail + Google Calendar) read/sync/write provider.
//!
//! Google is the cloud-API counterpart to `provider-graph` (OAuth bearer + JSON over
//! HTTP), housing *both* mail (Gmail) and calendar (Google Calendar) behind one shared
//! HTTP transport, exactly as `provider-graph` houses Microsoft mail + calendar. The
//! two halves differ in sync shape in ways that make Google *simpler* than Graph:
//!
//! - **Gmail mail sync is account-global.** Gmail's `historyId` is an account-wide incremental
//!   cursor (like JMAP's per-account `Email` state, unlike Graph's per-folder `delta` or IMAP's
//!   per-mailbox state), so all of an account's messages sync under one
//!   [`SyncScope::GmailMessages`](engine_core::sync::SyncScope) — there is **no** per-label
//!   fan-out. Labels are **multi-membership** on the message itself (`labelIds`), synced under
//!   [`SyncScope::GmailLabelList`](engine_core::sync::SyncScope).
//! - **Google Calendar is IANA-native** (event times carry an IANA `timeZone`, so no Windows-zone
//!   table is needed) and returns **recurring masters with an RFC 5545 `RRULE`** (the master + rule
//!   + local-expansion model the engine wants), unlike Graph's pre-expanded `calendarView`.
//!
//! # Layers
//!
//! - `error` — [`GoogleError`] and its classification into the engine's `FailureClass`.
//! - `base64url` — the URL-safe base64 codec Gmail's `raw` message field uses.
//! - `json` — pure `serde_json::Value` extraction helpers.
//! - `transport` — bearer HTTP behind the `GoogleTransport` seam ([`GoogleClient`]).
//!
//! The mail and calendar read/sync, write, and submission layers land as slices
//! (Phases B–E of the Google plan); this module set is the shared transport spine.

mod base64url;
mod cal_fetch;
mod cal_normalize;
mod cal_write;
mod calendar;
mod error;
mod fetch;
mod http_transport;
mod json;
mod mutate;
mod normalize;
mod provider;
mod submit;
mod transport;

#[cfg(test)]
mod test_support;

pub use cal_fetch::CalendarWindow;
pub use calendar::GoogleCalendarProvider;
pub use error::GoogleError;
pub use provider::GmailProvider;
pub use transport::GoogleClient;
