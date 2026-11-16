//! The fetch-window half of [`Capabilities`]: whether a transport fetches a calendar's events
//! whole, or only those inside a date window its cursor was opened over.

use crate::Capabilities;

impl Capabilities {
    /// Marks the transport's event fetch as **windowed**: a snapshot lists only the events inside
    /// the date window the engine names in [`Provider::sync_events`](crate::Provider::sync_events),
    /// so the stored calendar is complete only inside the window its cursor was opened over.
    ///
    /// Graph's `calendarView/delta` takes a mandatory range and its `deltaLink` reports changes
    /// only inside it; Google's `events.list` takes `timeMin`/`timeMax` for the snapshot its
    /// `syncToken` continues, and that token reports changes past them too (both measured), but
    /// neither ever lists an event that was there before and did not change. CalDAV
    /// and JMAP fetch a calendar whole, so they never advertise this, and the engine never has
    /// to fetch a range of theirs twice.
    #[must_use]
    pub const fn with_windowed_events(mut self) -> Self {
        self.windowed_events = true;
        self
    }

    /// Whether a calendar's stored events are complete only inside the window its cursor was
    /// opened over ([`with_windowed_events`](Self::with_windowed_events)).
    #[must_use]
    pub const fn windowed_events(self) -> bool {
        self.windowed_events
    }
}
