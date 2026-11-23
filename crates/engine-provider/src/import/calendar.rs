//! Importing events: what an event import sends, what it answers, the limits a calendar states,
//! and the [`CalendarImporter`] trait.
//!
//! It mirrors the mail half wherever the two mean the same thing: one answer per event, in order;
//! `requires_confirmation` for a write that went out unanswered; `item_unavailable` for an event
//! the target will never take; one [`check`](CalendarImportLimits::check) every adapter refuses
//! by; a cursor read before a batch's first write. Where they differ, the difference is the
//! data's:
//!
//! - **An event is one document, and its state is inside it.** A message carries state beside its
//!   bytes (keywords, an arrival time), which a target may keep or drop field by field, so a mail
//!   receipt says what was kept. An event's alarms, attendees and recurrence are lines of its
//!   iCalendar, stored whole, so there is nothing beside the bytes to keep or drop. A target that
//!   stores less of the document than it was given (none observed: every surveyed server reads back
//!   the same projection) is the case a `kept` on [`ImportedEvent`] will exist for, added with the
//!   first adapter that has one.
//! - **An event has an identity of its own, its `UID`.** A message is matched by fingerprint; an
//!   event is matched by `UID`, so an event the calendar already holds under that `UID` is [already
//!   there](ImportedEvent::already_there), never stored twice.
//! - **Importing tells nobody.** Storing a meeting on a server that schedules would send its
//!   invitations, so an importer stores it as one nobody is to be told about (on CalDAV,
//!   `SCHEDULE-AGENT=CLIENT` on every party). That is part of the contract, not an option.

use std::{num::NonZeroU64, sync::Arc};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, CalendarId, EventId},
    raw::RawIcal,
    sync::SyncState,
    version::RevisionTokens,
};

use crate::{ProviderError, ProviderResult};

/// One event to import: its iCalendar, exactly as the source held it, and the calendar it goes
/// in.
///
/// `ical` is one calendar object: one event with its overrides, sharing one `UID` (RFC 4791
/// §4.1). The importer stores it as it is, apart from what keeps the import silent, so the target
/// holds the event the source held. `Debug` prints the size, never the text ([`RawIcal`]).
///
/// Built with [`new`](Self::new), so a part added later does not break a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventImport {
    /// The event, as iCalendar (RFC 5545).
    pub ical: RawIcal,
    /// The calendar to store it in.
    pub calendar: CalendarId,
}

impl EventImport {
    /// The event `ical`, to store in `calendar`.
    #[must_use]
    pub fn new(ical: RawIcal, calendar: CalendarId) -> Self {
        Self { ical, calendar }
    }
}

/// What importing one event answers.
///
/// Built with [`created`](Self::created) or [`existing`](Self::existing), so a part added later
/// does not break an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportedEvent {
    /// The event the target holds, where its answer names it: the stored event's id, or the one
    /// that already held the `UID`.
    pub key: Option<EventId>,
    /// Whether the calendar already held an event with this `UID` and stored nothing new. The
    /// event is there either way, as the calendar held it.
    pub already_there: bool,
    /// The revision the target reported for what it stored (a CalDAV `ETag`); empty where it
    /// reported none, or stored nothing.
    pub revisions: RevisionTokens,
}

impl ImportedEvent {
    /// An event the import stored, as `key`, at `revisions`.
    #[must_use]
    pub fn created(key: EventId, revisions: RevisionTokens) -> Self {
        Self {
            key: Some(key),
            already_there: false,
            revisions,
        }
    }

    /// An event the calendar already held under the same `UID`, as `key` where the target named
    /// it.
    #[must_use]
    pub fn existing(key: Option<EventId>) -> Self {
        Self {
            key,
            already_there: true,
            revisions: RevisionTokens::none(),
        }
    }
}

/// The limits a calendar states for the events it imports.
///
/// As for mail, what a target states is not all it enforces: an event inside these limits can
/// still be refused, and the importer answers that refusal for that event alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CalendarImportLimits {
    max_event_size: Option<NonZeroU64>,
}

impl CalendarImportLimits {
    /// A calendar that states no limit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The calendar takes no event larger than `bytes` (builder-style): CalDAV
    /// `max-resource-size` (RFC 4791 §5.2.5).
    #[must_use]
    pub fn with_max_event_size(mut self, bytes: NonZeroU64) -> Self {
        self.max_event_size = Some(bytes);
        self
    }

    /// The largest event the calendar takes, in bytes, or `None` where it states none.
    #[must_use]
    pub fn max_event_size(&self) -> Option<NonZeroU64> {
        self.max_event_size
    }

    /// Checks `event` against these limits.
    ///
    /// # Errors
    ///
    /// An event larger than the calendar takes is [`ProviderError::item_unavailable`]: the
    /// calendar will never take it, and the rest of the batch is unaffected.
    pub fn check(&self, event: &EventImport) -> ProviderResult<()> {
        let size = u64::try_from(event.ical.as_str().len()).unwrap_or(u64::MAX);
        if let Some(max) = self.max_event_size
            && size > max.get()
        {
            return Err(ProviderError::item_unavailable(format!(
                "a {size}-byte event exceeds the calendar's {max}-byte limit"
            )));
        }
        Ok(())
    }
}

/// Imports events into a target account's calendars.
#[async_trait]
pub trait CalendarImporter: Send + Sync {
    /// The limits `calendar` states for what it imports ([`CalendarImportLimits`]).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] when the target cannot be asked, or does not know
    /// `calendar`.
    async fn calendar_import_limits(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<CalendarImportLimits>;

    /// The target's cursor for `calendar`, read now: a delta from it, through the target's own
    /// [`CalendarSync::sync_events`](crate::CalendarSync::sync_events), lists every event stored
    /// in `calendar` after this call.
    ///
    /// A host records it before a batch's first write, so after a crash it can find what that
    /// batch stored without reading the calendar whole.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] when the target cannot be asked, or does not know
    /// `calendar`.
    async fn import_cursor(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<SyncState>;

    /// Imports `batch`, answering each event in order: the answer at index `i` is the outcome of
    /// `batch[i]`, and there are as many answers as events. Nobody is told: no invitation, reply
    /// or cancellation leaves the target because of an import.
    ///
    /// What an answer means:
    ///
    /// - `Ok`: the event is in its calendar, stored now or [already
    ///   there](ImportedEvent::already_there) under the same `UID`.
    /// - An error with [`requires_confirmation`](ProviderError::requires_confirmation): the event
    ///   **may** have been stored. Find it (from the [`import_cursor`](Self::import_cursor), or by
    ///   its `UID`) before sending it again.
    /// - Any other error: the event was not stored. One that is
    ///   [`item_unavailable`](ProviderError::item_unavailable) will never be (too large, not one
    ///   event, refused by the server); one that is retryable can be sent again.
    async fn import_events(
        &self,
        account: &AccountId,
        batch: &[EventImport],
    ) -> Vec<ProviderResult<ImportedEvent>>;
}

/// A boxed importer is an importer, as for [`MailImporter`](super::MailImporter).
#[async_trait]
impl<T: CalendarImporter + ?Sized> CalendarImporter for Box<T> {
    async fn calendar_import_limits(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<CalendarImportLimits> {
        (**self).calendar_import_limits(account, calendar).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<SyncState> {
        (**self).import_cursor(account, calendar).await
    }

    async fn import_events(
        &self,
        account: &AccountId,
        batch: &[EventImport],
    ) -> Vec<ProviderResult<ImportedEvent>> {
        (**self).import_events(account, batch).await
    }
}

/// A shared importer is an importer, as for [`Box`].
#[async_trait]
impl<T: CalendarImporter + ?Sized> CalendarImporter for Arc<T> {
    async fn calendar_import_limits(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<CalendarImportLimits> {
        (**self).calendar_import_limits(account, calendar).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<SyncState> {
        (**self).import_cursor(account, calendar).await
    }

    async fn import_events(
        &self,
        account: &AccountId,
        batch: &[EventImport],
    ) -> Vec<ProviderResult<ImportedEvent>> {
        (**self).import_events(account, batch).await
    }
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
