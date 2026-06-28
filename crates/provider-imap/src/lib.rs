//! `provider-imap` — the IMAP (RFC 9051) read/sync provider, with SMTP (RFC 5321)
//! submission.
//!
//! This is the legacy-mail counterpart to `provider-jmap`: where JMAP is one
//! stateless HTTP session, IMAP is a **stateful, per-mailbox** protocol and SMTP a
//! separate submission transport. The crate is a hand-rolled minimal client over a
//! generic async stream — no third-party IMAP/SMTP library — so the SMTP
//! per-recipient and post-DATA invariants stay under our control and the whole
//! protocol is offline-testable by replaying captured transcripts through an
//! in-memory stream (mirroring the harness probe and `provider-jmap`'s executor
//! seam). It implements the [`engine_provider::Provider`] contract so the sync
//! orchestrator never switches on provider kind.
//!
//! # Shape (and how it differs from JMAP)
//!
//! - **Email scope is per mailbox.** A JMAP account has one `Email` scope; an IMAP
//!   account has one [`SyncScope::ImapMailbox`](engine_core::sync::SyncScope) per
//!   folder. So an [`ImapProvider`] is **bound to a single mailbox** for email:
//!   its [`email_scope`](engine_provider::Provider::email_scope) names that
//!   mailbox, and [`sync_email_page`](engine_provider::Provider::sync_email_page)
//!   is a UID-window `FETCH` over it. The folder list syncs under the per-account
//!   [`SyncScope::ImapMailboxList`](engine_core::sync::SyncScope). The cross-folder
//!   fan-out is the later orchestrator's job.
//! - **Identity is synthesized.** A mail object's key is `(mailbox, UIDVALIDITY,
//!   UID)`, so an IMAP copy in another folder is a **distinct** object with a
//!   single membership (contrast JMAP, where the same copy is one object with two
//!   memberships). `Message-ID` is a hint, never identity.
//! - **A UIDVALIDITY reset is a snapshot.** When the mailbox's UID space is
//!   renumbered, every old key is invalid; the pass becomes a snapshot (rediscovery)
//!   that tombstones the stale rows — the IMAP analogue of `cannotCalculateChanges`.
//!
//! # Layers
//!
//! - `transport` — connect (TCP + injected `tokio-rustls` connector) and the
//!   tagged line protocol over any `AsyncRead + AsyncWrite` stream.
//! - `parse` — pure response parsers (`SELECT`/`SEARCH`/`FETCH`/`ENVELOPE`/`LIST`);
//!   hostile input is rejected, never panicked on.
//! - `mail` — normalize parsed rows into [`Message`](engine_core::mail::Message) /
//!   [`Mailbox`](engine_core::mail::Mailbox).
//! - `cursor` — the per-mailbox `SyncState` (UIDVALIDITY/UIDNEXT, plus an optional
//!   QRESYNC `HIGHESTMODSEQ`) and opaque [`PageToken`](engine_provider::PageToken)
//!   encodings.
//! - `sync` — the snapshot/delta + UID-window paging orchestration.
//! - `qresync` — the QRESYNC incremental delta (RFC 7162): flag changes + expunges of
//!   already-synced mail via `CHANGEDSINCE`/`VANISHED`, when the session negotiates it.
//! - `mutate` — applying a [`MailEdit`](engine_provider::MailEdit) (`UID
//!   STORE`/`MOVE`/`EXPUNGE`) to the bound mailbox.
//! - `filing` — SMTP submission + `APPEND` filing of sent copies and drafts.
//! - `provider` — [`ImapProvider`], the [`Provider`](engine_provider::Provider) impl.
//!
//! Tier-1 metadata only: like step 4, the raw RFC 5322 body is not materialized
//! yet (durable blob storage is a later store sub-step).

mod base64;
mod cursor;
mod encoded_word;
mod error;
mod fetch;
mod filing;
mod mail;
mod mutate;
mod parse;
mod parse_qresync;
mod provider;
mod qresync;
mod smtp;
mod sync;
mod target;
mod tokenize;
mod transport;

#[cfg(test)]
mod integration;
#[cfg(test)]
mod mock;

pub use error::ImapError;
pub use provider::{ImapConfig, ImapProvider};
