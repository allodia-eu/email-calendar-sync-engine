//! The date floor an email cursor was fetched under, stored with it.
//!
//! A cursor only goes on from what its pass fetched, so one written under a later floor than a
//! pass asks for knows nothing of the mail between the two floors, and that pass starts the
//! folder over. A host clears the cursors when it widens the window, but a pass that began under
//! the old window can still claim a folder after the clear and write a cursor for it; this is
//! what keeps that cursor from standing.
//!
//! A pass asking for a **later** floor resumes: a host computes its floor from today, so the
//! floor moves on every day with nothing new to fetch. Mail a narrower window no longer wants is
//! left to the delta filter (`SyncWindow::admits`) and to `prune_account_mail_outside_window`.
//!
//! A pass over the full history writes the provider's cursor bare, so a bare cursor reads as
//! one fetched under no floor at all, which is also how every cursor written before floors were
//! recorded reads.

use engine_core::{
    sync::{SyncState, SyncWindow},
    time::CalendarDate,
};

const SINCE: &str = "since=";
const END: char = '\n';

/// The provider's cursor to resume `window` from, or `None` when `stored` was written under a
/// later floor and the folder starts over.
pub(crate) fn resume(stored: Option<SyncState>, window: SyncWindow) -> Option<SyncState> {
    let (floor, cursor) = split(stored?);
    let covers = match (window.floor(), floor) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(asked), Some(fetched)) => asked >= fetched,
    };
    covers.then_some(cursor)
}

/// `cursor` as stored by a pass over `window`.
pub(crate) fn stamp(cursor: SyncState, window: SyncWindow) -> SyncState {
    match window.floor() {
        None => cursor,
        Some(floor) => SyncState::new(format!("{SINCE}{floor}{END}{}", cursor.as_str())),
    }
}

/// The floor a stored cursor was fetched under and the provider's cursor. A cursor that only
/// looks stamped is the provider's own, whole.
fn split(stored: SyncState) -> (Option<CalendarDate>, SyncState) {
    let stamped = stored
        .as_str()
        .strip_prefix(SINCE)
        .and_then(|rest| rest.split_once(END))
        .and_then(|(floor, cursor)| Some((floor.parse().ok()?, SyncState::new(cursor))));
    match stamped {
        Some((floor, cursor)) => (Some(floor), cursor),
        None => (None, stored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn since(month: u8) -> SyncWindow {
        SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap())
    }

    fn resumed(stored: &SyncState, window: SyncWindow) -> Option<String> {
        resume(Some(stored.clone()), window).map(|c| c.as_str().to_owned())
    }

    #[test]
    fn a_full_window_stores_the_cursor_bare() {
        let stored = stamp(SyncState::new("c1"), SyncWindow::full());
        assert_eq!(stored.as_str(), "c1");
        assert_eq!(resumed(&stored, SyncWindow::full()).as_deref(), Some("c1"));
        assert_eq!(resumed(&stored, since(4)).as_deref(), Some("c1"));
    }

    #[test]
    fn a_floor_resumes_for_itself_and_later_floors_only() {
        let stored = stamp(SyncState::new("c1"), since(4));
        assert_eq!(stored.as_str(), "since=2026-04-01\nc1");
        assert_eq!(resumed(&stored, since(4)).as_deref(), Some("c1"));
        assert_eq!(resumed(&stored, since(5)).as_deref(), Some("c1"));
        assert_eq!(resumed(&stored, since(3)), None);
        assert_eq!(resumed(&stored, SyncWindow::full()), None);
    }

    #[test]
    fn a_provider_cursor_that_only_looks_stamped_is_kept_whole() {
        for raw in ["since=", "since=soon\nc1", "since=2026-04-01"] {
            let stored = SyncState::new(raw);
            assert_eq!(resumed(&stored, SyncWindow::full()).as_deref(), Some(raw));
        }
    }
}
