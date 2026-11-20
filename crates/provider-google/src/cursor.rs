//! Where a Gmail message sync stands between passes, written into the scope's cursor.
//!
//! A large mailbox takes many passes to fetch: Gmail bills 20 units a message against 6,000 per
//! user per minute (`crate::quota`), so a backfill runs at about 270 messages a minute, and an
//! app is suspended, quit or moved between networks long before a large mailbox is done. The
//! cursor therefore records how far the backfill got, and the next pass carries on from there.
//!
//! The page token is the only position that can: `messages.list` is not ordered strictly by
//! date (measured on the test account, 3 of 193 messages out of date order, one by 13 days), so
//! a date boundary would skip mail. A token the server no longer accepts restarts the backfill.

use engine_core::sync::SyncState;
use engine_provider::PageToken;

const BACKFILL: &str = "backfill;h=";
const RECONCILE: &str = "reconcile;h=";
const PAGE: &str = ";p=";

/// Where a Gmail message sync stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GmailCursor {
    /// Fully synced as of `history`; the next pass is a delta from it. Stored as the bare
    /// historyId, the shape every cursor had before backfills existed.
    Delta { history: SyncState },
    /// Fetching the window a page at a time: deltas from `history` keep what is already stored
    /// current, and the listing continues from `page` (`None` is its first page).
    Backfill {
        history: SyncState,
        page: Option<PageToken>,
    },
    /// Every page fetched; the next pass lists the window's ids to tombstone what the server no
    /// longer has, and ends on `history`.
    Reconcile { history: SyncState },
}

impl GmailCursor {
    /// Reads a stored cursor, or `None` for one this build cannot read, which starts the
    /// backfill over rather than guessing where it stood.
    pub(crate) fn read(state: &SyncState) -> Option<Self> {
        let raw = state.as_str();
        if let Some(rest) = raw.strip_prefix(BACKFILL) {
            let (history, page) = rest.split_once(PAGE)?;
            return Some(Self::Backfill {
                history: history_id(history)?,
                page: (!page.is_empty()).then(|| PageToken::new(page)),
            });
        }
        if let Some(history) = raw.strip_prefix(RECONCILE) {
            return Some(Self::Reconcile {
                history: history_id(history)?,
            });
        }
        Some(Self::Delta {
            history: history_id(raw)?,
        })
    }

    /// The cursor to store.
    pub(crate) fn state(&self) -> SyncState {
        match self {
            Self::Delta { history } => history.clone(),
            Self::Backfill { history, page } => SyncState::new(format!(
                "{BACKFILL}{}{PAGE}{}",
                history.as_str(),
                page.as_ref().map_or("", PageToken::as_str),
            )),
            Self::Reconcile { history } => {
                SyncState::new(format!("{RECONCILE}{}", history.as_str()))
            }
        }
    }
}

/// A historyId as Gmail writes one: a run of digits.
fn history_id(raw: &str) -> Option<SyncState> {
    (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit())).then(|| SyncState::new(raw))
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
