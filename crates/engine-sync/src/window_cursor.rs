//! What an email pass does with the depth its stored cursor covers.
//!
//! Every email cursor is stored with the depth it covers and, while a widening runs, the depth it
//! widens to and whether its band has begun ([`StampedCursor`], in `engine-core`, so a store can
//! read the record too). This is the decision that record drives
//! (`docs/design/windows-and-widening.md`, §4.3):
//!
//! | Stored record against the depth asked | Pass |
//! |---|---|
//! | No cursor | A fresh pass under the depth asked. |
//! | The covered depth covers what is asked (the daily floor move included) | A delta, stamped with the depth asked; any widening is dropped. |
//! | A widening whose band has begun, toward a depth that covers the one asked | A delta, then [`Provider::widen_email`] **resuming** it: the adapter continues from the position its cursor recorded. |
//! | Anything else reaching further back | A delta, then [`Provider::widen_email`] from the covered depth to the one asked, **afresh**: a position the cursor recorded belongs to another widening, and is ignored. |
//!
//! **Continuing is decided here, never by an adapter.** Only this record knows what ran on the
//! scope between two passes. A pass asking a narrower depth drops the widening from the record,
//! and its delta may have dropped mail the band had not reached (an old message filed into the
//! folder meanwhile); a position recorded before it would skip that mail for good. And a reopen
//! (the default `widen_email`) has no position of its own to compare, only a cursor that is either
//! its fresh pass's or the delta's. So a widening resumes only when the record says its band
//! began and nothing narrower ran since; "toward a depth that covers the one asked" lets a window
//! computed from today, whose floor moves on daily, keep resuming its band rather than restart it.
//!
//! The depth is the scope's own: a folder-bound scope (IMAP, Graph) records its folder's window,
//! and an account-wide scope (JMAP, Gmail) records the account's and every mailbox's held further
//! back ([`MailboxWindows::deepened`]). So deepening one mailbox widens the one scope that fetches
//! it. A covered depth may claim less than the store holds, never more: it becomes the target
//! only in the commit that ends the widening.
//!
//! [`Provider::widen_email`]: engine_provider::Provider::widen_email

use engine_core::sync::{MailboxWindows, StampedCursor, SyncState, Widening};

/// What one email pass runs, decided from its stored cursor and the depth it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// No cursor: a fresh pass under the depth asked.
    Fresh,
    /// A delta from the provider's cursor, which covers the depth asked.
    Delta(SyncState),
    /// A delta from the provider's cursor, then the widening from the depth it covers.
    Widen {
        /// The provider's cursor, as stored.
        cursor: SyncState,
        /// From the covered depth to the one asked.
        widening: Widening,
    },
}

impl Plan {
    /// The pass for a scope whose stored cursor is `stored`, asked to hold `asked`.
    pub(crate) fn new(stored: Option<SyncState>, asked: &MailboxWindows) -> Self {
        let Some(stored) = stored else {
            return Self::Fresh;
        };
        let stored = StampedCursor::parse(stored);
        let resumes =
            stored.is_band_begun() && stored.target().is_some_and(|target| target.covers(asked));
        match Widening::new(stored.covered().clone(), asked.clone()) {
            None => Self::Delta(stored.into_cursor()),
            Some(widening) => Self::Widen {
                cursor: stored.into_cursor(),
                widening: if resumes {
                    widening.resuming()
                } else {
                    widening
                },
            },
        }
    }

    /// The provider's cursor the first stream starts from, and the widening to run after it.
    pub(crate) fn into_parts(self) -> (Option<SyncState>, Option<Widening>) {
        match self {
            Self::Fresh => (None, None),
            Self::Delta(cursor) => (Some(cursor), None),
            Self::Widen { cursor, widening } => (Some(cursor), Some(widening)),
        }
    }
}

/// Which stream of a pass committed a cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The pass's first stream: a fresh pass or a delta.
    First,
    /// The widening's band.
    Band,
}

/// `cursor` as a pass asking for `asked` stores it: covering `asked`, or, while `widening` runs,
/// covering what it widens from with what it widens to beside it, its band begun once the band
/// has committed a cursor, or all along when the widening resumes one that had.
pub(crate) fn stamp(
    cursor: SyncState,
    asked: &MailboxWindows,
    widening: Option<&Widening>,
    phase: Phase,
) -> SyncState {
    let Some(widening) = widening else {
        return StampedCursor::new(cursor, asked.clone()).to_state();
    };
    let stamped =
        StampedCursor::new(cursor, widening.from().clone()).widening_to(widening.to().clone());
    if phase == Phase::Band || widening.resumes() {
        stamped.band_begun()
    } else {
        stamped
    }
    .to_state()
}

/// The provider's own cursor inside a stored one.
pub(crate) fn provider_cursor(stored: SyncState) -> SyncState {
    StampedCursor::parse(stored).into_cursor()
}

#[cfg(test)]
#[path = "window_cursor_tests.rs"]
mod tests;
