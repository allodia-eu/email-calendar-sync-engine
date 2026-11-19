//! The Gmail message cursor survives a round trip through the store in every form, and a
//! cursor written before backfills existed still reads as the delta it always was.

use engine_core::sync::SyncState;
use engine_provider::PageToken;

use super::GmailCursor;

fn history(id: &str) -> SyncState {
    SyncState::new(id)
}

#[test]
fn a_bare_history_id_is_a_delta_cursor_as_it_always_was() {
    // Every cursor stored before backfills existed has this shape, and must not restart a sync.
    assert_eq!(
        GmailCursor::read(&SyncState::new("40656")),
        Some(GmailCursor::Delta {
            history: history("40656")
        })
    );
}

#[test]
fn every_form_round_trips_through_the_store() {
    for cursor in [
        GmailCursor::Delta {
            history: history("40656"),
        },
        GmailCursor::Backfill {
            history: history("40656"),
            page: None,
        },
        GmailCursor::Backfill {
            history: history("40656"),
            page: Some(PageToken::new("09876543210123456789")),
        },
        GmailCursor::Reconcile {
            history: history("40656"),
        },
    ] {
        assert_eq!(
            GmailCursor::read(&cursor.state()),
            Some(cursor.clone()),
            "{cursor:?}"
        );
    }
}

#[test]
fn a_page_token_is_kept_whole_whatever_it_contains() {
    // Google documents the token as opaque, so nothing in it may be read as a separator.
    let cursor = GmailCursor::Backfill {
        history: history("7"),
        page: Some(PageToken::new("a;b=c;h=9")),
    };
    assert_eq!(GmailCursor::read(&cursor.state()), Some(cursor));
}

#[test]
fn a_cursor_this_build_cannot_read_starts_the_backfill_over() {
    for unreadable in ["backfill;p=1", "reconcile;", "backfill;h=;p=", ""] {
        assert_eq!(
            GmailCursor::read(&SyncState::new(unreadable)),
            None,
            "{unreadable:?}"
        );
    }
}
