//! The sync depth window.

use serde::{Deserialize, Serialize};

use crate::time::{CalendarDate, UtcDateTime};

/// How far back a mail sync fetches — the "sync depth" a host bounds a first
/// snapshot/backfill to, so a large mailbox syncs only recent mail.
///
/// It is a **per-sync argument**, not a provider-construction detail: a host
/// changes depth by passing a different window to the next sync, without
/// reconnecting providers. Provider-neutral — each adapter maps `since` to its
/// protocol's date filter: IMAP `UID SEARCH SINCE <date>`, JMAP an `Email/query`
/// `after` filter on `receivedAt`, Microsoft Graph a `receivedDateTime ge` filter.
///
/// It bounds what a pass **fetches** and, through [`admits`](Self::admits), what the engine
/// **stores**. The two are not the same job: a delta's "new arrivals" are new to *us*, not
/// necessarily recent. IMAP has no in-place edit, so filing a three-year-old message into a
/// folder mints a UID above the cursor and the delta carries it as an arrival; Graph, Gmail and
/// JMAP deltas report a moved message the same way. Left unfiltered, mail the user asked this
/// device not to keep walks back in through every protocol, and no later pass removes it —
/// a delta never re-lists what it did not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncWindow {
    /// The oldest date to fetch, inclusive, or `None` for the full history.
    pub since: Option<CalendarDate>,
}

impl SyncWindow {
    /// A window with no floor — the entire mailbox history.
    #[must_use]
    pub fn full() -> Self {
        Self { since: None }
    }

    /// A window bounded to mail delivered on or after `since`.
    #[must_use]
    pub fn since(since: CalendarDate) -> Self {
        Self { since: Some(since) }
    }

    /// The date floor, if any.
    #[must_use]
    pub fn floor(&self) -> Option<CalendarDate> {
        self.since
    }

    /// Whether this window bounds the sync at all (`false` for the full history).
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.since.is_some()
    }

    /// Whether a message dated `date` belongs inside this window.
    ///
    /// `date` is the message's `received_at` falling back to `sent_at` — the same value
    /// `MailRow::date_utc` carries and every adapter maps its protocol's date filter to, so
    /// what a pass fetches and what the engine keeps cannot disagree.
    ///
    /// Two rules, both deliberate:
    ///
    /// - The floor is **inclusive** and compared as a **date**, not an instant. A message sent at
    ///   any hour of the floor day is inside. A provider's date filter has date granularity (IMAP
    ///   `SINCE` takes `dd-Mon-yyyy`), so an instant comparison here would reject mail the fetch
    ///   deliberately asked for.
    /// - **Undated mail is admitted.** A message with neither timestamp is not provably outside the
    ///   window, and dropping it would lose mail on the strength of a missing header.
    ///
    /// An unbounded window admits everything.
    #[must_use]
    pub fn admits(&self, date: Option<UtcDateTime>) -> bool {
        let (Some(floor), Some(date)) = (self.since, date) else {
            return true;
        };
        CalendarDate::new(date.year(), date.month(), date.day()).is_ok_and(|date| date >= floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_window_has_no_floor() {
        let window = SyncWindow::full();
        assert_eq!(window, SyncWindow::default());
        assert!(!window.is_bounded());
        assert_eq!(window.floor(), None);
    }

    #[test]
    fn bounded_window_carries_its_floor() {
        let floor = CalendarDate::new(2026, 1, 7).unwrap();
        let window = SyncWindow::since(floor);
        assert!(window.is_bounded());
        assert_eq!(window.floor(), Some(floor));
    }

    #[test]
    fn roundtrips_through_json() {
        let window = SyncWindow::since(CalendarDate::new(2026, 1, 7).unwrap());
        let json = serde_json::to_string(&window).unwrap();
        assert_eq!(json, r#"{"since":"2026-01-07"}"#);
        assert_eq!(serde_json::from_str::<SyncWindow>(&json).unwrap(), window);
    }

    #[test]
    fn admits_mail_on_or_after_an_inclusive_floor() {
        let window = SyncWindow::since(CalendarDate::new(2026, 4, 1).unwrap());
        let at = |text: &str| Some(text.parse::<UtcDateTime>().unwrap());

        assert!(!window.admits(at("2026-03-31T23:59:59Z")), "day before");
        // The floor is a date, so any hour of it is inside — a provider's own filter has
        // date granularity, and an instant comparison would drop mail the fetch asked for.
        assert!(window.admits(at("2026-04-01T00:00:00Z")), "floor midnight");
        assert!(
            window.admits(at("2026-04-01T23:59:59Z")),
            "floor end of day"
        );
        assert!(window.admits(at("2026-06-20T09:00:00Z")), "well inside");
    }

    #[test]
    fn undated_mail_and_an_unbounded_window_admit_everything() {
        let bounded = SyncWindow::since(CalendarDate::new(2026, 4, 1).unwrap());
        // Neither timestamp: not provably outside, so keeping it is the only safe answer.
        assert!(bounded.admits(None));
        // No floor: nothing is outside it.
        let old = Some("1999-01-01T00:00:00Z".parse::<UtcDateTime>().unwrap());
        assert!(SyncWindow::full().admits(old));
        assert!(SyncWindow::full().admits(None));
    }
}
