//! The sync depth window.

use serde::{Deserialize, Serialize};

use crate::time::CalendarDate;

/// How far back a mail sync fetches — the "sync depth" a host bounds a first
/// snapshot/backfill to, so a large mailbox syncs only recent mail.
///
/// It is a **per-sync argument**, not a provider-construction detail: a host
/// changes depth by passing a different window to the next sync, without
/// reconnecting providers. Provider-neutral — each adapter maps `since` to its
/// protocol's date filter: IMAP `UID SEARCH SINCE <date>`, JMAP an `Email/query`
/// `after` filter on `receivedAt`, Microsoft Graph a `receivedDateTime ge` filter.
///
/// The window only bounds a **snapshot/backfill**. A delta is always
/// new-arrivals-only (recent by definition), so a window never narrows it
/// (`imap-smtp.md`).
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
}
