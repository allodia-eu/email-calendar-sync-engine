// SPDX-License-Identifier: MPL-2.0
//! Write-direction Calendar `ApplicationData` serialization ([MS-ASCAL]
//! §2.2) — the upload twin of `calendar.rs`'s downsync parse. M8 calendar
//! upsync Task 1; Tasks 2-3 wrap this into Sync Add/Change Commands.
//!
//! Token fidelity red line: every page-4 token below is REUSED from
//! `calendar.rs` (whose values were verified against
//! `docs/Exchange/MS-ASWBXML.txt` §2.1.2.1.5) — no token value is invented
//! here. Element value semantics per [MS-ASCAL] §2.2.2 and [MS-ASDTYPE]
//! (§2.1 boolean `"0"`/`"1"`, §2.7.2 Compact DateTime, §2.7.6 TimeZone).
//!
//! Canonical emission order (fixed, asserted by tests):
//! ```text
//! Timezone, AllDayEvent, StartTime, EndTime (always emitted),
//! Subject?, Location?, Body?, Sensitivity?, BusyStatus?, Reminder?,
//! Attendees?, Recurrence?
//! ```
//! `Option` fields are emitted only when `Some`; the `Attendees` container
//! is omitted when the list is empty.
//!
//! Server-managed on write — NEVER emitted ([MS-ASCAL] §2.2.2): `UID`
//! (§2.2.2.46), `DtStamp` (§2.2.2.18), `MeetingStatus` (§2.2.2.28),
//! `ResponseRequested` (§2.2.2.39), attendee `AttendeeStatus` (§2.2.2.5),
//! and `OrganizerEmail`/`OrganizerName` (§2.2.2.35/§2.2.2.36 — the
//! organizer is derived server-side from the mailbox owner; live probe
//! 2026-08-22: identical Adds WITH organizer fields are rejected with
//! per-item Status 6, without them accepted). The `CalendarEventWrite`
//! organizer fields remain for round-trip bookkeeping; the serializer
//! ignores them.
//!
//! Recurrence: `Recurrence { Type, Interval?, DayOfWeek?, DayOfMonth?,
//! WeekOfMonth?, MonthOfYear?, Until? XOR Occurrences? }` reusing the
//! parse-model [`CalendarRecurrence`](crate::calendar::CalendarRecurrence).
//! `no_end` is DERIVED, not a wire
//! token ([MS-ASCAL] §2.2.2.37.1) — never emitted; `Until` wins when both
//! end conditions are (invalidly) set, with a warning.
//!
//! Timezone: fixed-offset TZI blob only (design D6 — no DST rules on
//! write); see
//! [`build_fixed_offset_tzi_base64`](crate::calendar_write::build_fixed_offset_tzi_base64).

mod build;
mod model;
#[cfg(test)]
mod tests;
mod tzi;

pub use build::build_calendar_application_data;
pub use model::{CalendarEventWrite, CalendarWriteError};
pub use tzi::build_fixed_offset_tzi_base64;
