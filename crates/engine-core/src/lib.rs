//! `engine-core` — the pure domain model and cross-crate contracts for the PIM
//! sync engine.
//!
//! This crate is deliberately **I/O-free and async-free**. It defines the
//! normalized mail and calendar types, the identity newtypes that keep account,
//! object, and collection keys from being mixed by accident, and the contract
//! types (`SyncScope`, `SyncState`, `SyncUpdate`, `SearchCoverage`, `PendingOp`)
//! that both stores and sync orchestration consume. Network access, runtime,
//! storage, recurrence *expansion*, and text extraction live in other crates;
//! this crate only models the data and the invariants.
//!
//! # Design rules
//!
//! - **Provider object identity and collection membership are separate axes.**
//!   A stored mail object is a provider object, not a deduplicated RFC 5322
//!   message; membership in a mailbox/label/calendar is a set, modeled
//!   independently from identity. See [`ids`] and the mail/calendar modules.
//! - **Keywords, membership, and collection role are three distinct axes.**
//!   `$seen`/`\Flagged`/Gmail `STARRED` are keywords; folders/labels/calendars
//!   are membership; inbox/sent/drafts/trash are normalized roles.
//! - **Provider-native payloads are preserved** beside the normalized projection
//!   for lossless re-derivation. See [`raw`].
//! - **Time is one model** — an instant resolved through its zone, or wall-clock
//!   for floating time. End is always `start + duration`, never a stored end
//!   instant.
//!
//! The module layout follows one responsibility per file, mirroring the primary
//! specs (JMAP Core/Mail RFC 8620/8621, JSCalendar RFC 8984, iCalendar RFC 5545,
//! iTIP/iMIP RFC 5546/6047, IMAP RFC 9051, CalDAV RFC 4791/6638) and the
//! provider references (Gmail API, Microsoft Graph).

#[macro_use]
mod macros;

pub mod attachment;
pub mod calendar;
pub mod coverage;
pub mod error;
pub mod extended;
pub mod ids;
pub mod mail;
pub mod membership;
pub mod patch;
pub mod raw;
pub mod scheduling;
pub mod search_index;
pub mod sync;
pub mod time;
pub mod version;
pub mod write;
