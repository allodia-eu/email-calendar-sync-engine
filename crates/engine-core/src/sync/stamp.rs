//! An email cursor stored with the mail depth it covers, and the widening running on it.
//!
//! A provider's cursor only goes on from what its pass fetched, so a store must know how far back
//! that was before a pass resumes from it: a cursor fetched under a later floor than a pass asks
//! for knows nothing of the mail between the two. The depth is recorded with the cursor, in the
//! same commit:
//!
//! - **covered**: what the store holds for the scope. It never claims more than the store holds,
//!   and may claim less (a floor that moves on every day records a later one while the older mail
//!   stays until a prune).
//! - **target**: while a widening runs, the depth it widens to. The covered depth becomes the
//!   target only in the commit that ends the widening, so a pass killed mid-way never claims mail
//!   it did not fetch.
//! - **band begun**: whether the running widening's band has committed a cursor of its own. Only
//!   then does the cursor hold a position the next pass may continue from
//!   ([`Widening::resuming`](super::Widening::resuming)).
//!
//! The record and its codec live here rather than in the orchestrator so that a store can lower a
//! recorded depth without knowing the orchestrator's format, and so that every host reads one
//! format. What a pass does with it is the orchestrator's decision.
//!
//! # The stored form
//!
//! Lines before the provider's cursor, each ended by a line break:
//!
//! ```text
//! since=2026-04-01                 the covered account floor
//! deeper=2020-01-01 Sent           a mailbox covered further back (`*`: its whole history)
//! deeper=2025-04-01 \ntarget       the target account floor (`*`: the whole history)
//! deeper=* \ntarget Archive        a mailbox the target holds further back
//! deeper=* \nbegun                 the target's band has begun
//! <the provider's cursor>
//! ```
//!
//! The widening's lines are written as mailboxes held further back whose ids start with a line
//! break (written `\n`), which no server's mailbox id does. That is what makes the form **safe to
//! roll back past**: an engine that predates them reads them as mailboxes no pass asks for, so it
//! resumes from the same covered depth and hands its provider the same cursor, and writes them
//! away with its next stamp. A line of its own, which it would hand the provider as part of the
//! cursor, would break every pass on a provider that parses its cursor (a Graph `deltaLink`).
//!
//! A cursor covering the whole history is stored **bare**, so a bare cursor reads as covering the
//! whole history, which is also how every cursor written before depths were recorded reads. A
//! cursor that only looks stamped is the provider's own, kept whole.

use crate::{
    ids::MailboxId,
    sync::{MailboxWindows, SyncState, SyncWindow},
    time::CalendarDate,
};

const SINCE: &str = "since=";
const DEEPER: &str = "deeper=";
/// The floor of a window over its whole history.
const UNBOUNDED: &str = "*";
const END: char = '\n';
/// What starts the id of a record that is not a mailbox: a line break, which no mailbox id has.
const RECORD: char = '\n';
const TARGET: &str = "\ntarget";
const BEGUN: &str = "\nbegun";

/// A provider's email cursor, with the depth it covers and the widening running on it. See the
/// module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StampedCursor {
    covered: MailboxWindows,
    target: Option<MailboxWindows>,
    begun: bool,
    cursor: SyncState,
}

impl StampedCursor {
    /// `cursor`, fetched under `covered`, with no widening running.
    #[must_use]
    pub fn new(cursor: SyncState, covered: MailboxWindows) -> Self {
        Self {
            covered,
            target: None,
            begun: false,
            cursor,
        }
    }

    /// Records that a widening to `target` is running (builder-style). A target the covered
    /// depth already covers widens nothing, and is not recorded.
    #[must_use]
    pub fn widening_to(mut self, target: MailboxWindows) -> Self {
        self.target = (!self.covered.covers(&target)).then_some(target);
        self.begun &= self.target.is_some();
        self
    }

    /// Records that the running widening's band has begun (builder-style): the cursor holds the
    /// band's position, which a pass resuming the widening continues from. Nothing is recorded
    /// when no widening runs.
    #[must_use]
    pub fn band_begun(mut self) -> Self {
        self.begun = self.target.is_some();
        self
    }

    /// Reads a stored cursor. A bare cursor, or one that only looks stamped, covers the whole
    /// history and has no widening running.
    #[must_use]
    pub fn parse(stored: SyncState) -> Self {
        match parse(stored.as_str()) {
            Some(parsed) => parsed,
            None => Self::new(stored, MailboxWindows::default()),
        }
    }

    /// The form to store. See the module docs.
    #[must_use]
    pub fn to_state(&self) -> SyncState {
        let Some(floor) = self.covered.account().floor() else {
            return self.cursor.clone();
        };
        let mut stamped = format!("{SINCE}{floor}{END}");
        for (mailbox, window) in self.covered.deepened() {
            line(&mut stamped, window, mailbox.as_str());
        }
        if let Some(target) = &self.target {
            line(&mut stamped, target.account(), TARGET);
            for (mailbox, window) in target.deepened() {
                line(
                    &mut stamped,
                    window,
                    &format!("{TARGET} {}", mailbox.as_str()),
                );
            }
            if self.begun {
                line(&mut stamped, SyncWindow::full(), BEGUN);
            }
        }
        stamped.push_str(self.cursor.as_str());
        SyncState::new(stamped)
    }

    /// The depth the store holds for the scope.
    #[must_use]
    pub fn covered(&self) -> &MailboxWindows {
        &self.covered
    }

    /// The depth a running widening widens to, or `None` when none is running.
    #[must_use]
    pub fn target(&self) -> Option<&MailboxWindows> {
        self.target.as_ref()
    }

    /// Whether the running widening's band has begun ([`band_begun`](Self::band_begun)).
    #[must_use]
    pub fn is_band_begun(&self) -> bool {
        self.begun
    }

    /// The provider's own cursor.
    #[must_use]
    pub fn cursor(&self) -> &SyncState {
        &self.cursor
    }

    /// The provider's own cursor, by value.
    #[must_use]
    pub fn into_cursor(self) -> SyncState {
        self.cursor
    }
}

/// One `deeper=` line naming `id` under `window`'s floor.
fn line(out: &mut String, window: SyncWindow, id: &str) {
    out.push_str(DEEPER);
    out.push_str(
        &window
            .floor()
            .map_or_else(|| UNBOUNDED.to_owned(), |floor| floor.to_string()),
    );
    out.push(' ');
    out.push_str(&escape(id));
    out.push(END);
}

fn parse(raw: &str) -> Option<StampedCursor> {
    let (floor, mut rest) = raw.strip_prefix(SINCE)?.split_once(END)?;
    let mut covered = MailboxWindows::new(SyncWindow::since(floor.parse().ok()?));
    let mut target: Option<MailboxWindows> = None;
    let mut begun = false;
    while let Some(line) = rest.strip_prefix(DEEPER) {
        let (entry, after) = line.split_once(END)?;
        let (floor, id) = entry.split_once(' ')?;
        let window = window(floor)?;
        let id = unescape(id)?;
        if !id.starts_with(RECORD) {
            covered = covered.deepen(MailboxId::try_from(id.as_str()).ok()?, window);
        } else if id == BEGUN {
            begun = true;
        } else if id == TARGET {
            target = Some(MailboxWindows::new(window));
        } else {
            let mailbox = id.strip_prefix(TARGET)?.strip_prefix(' ')?;
            let mailbox = MailboxId::try_from(mailbox).ok()?;
            target = Some(target?.deepen(mailbox, window));
        }
        rest = after;
    }
    Some(StampedCursor {
        begun: begun && target.is_some(),
        covered,
        target,
        cursor: SyncState::new(rest),
    })
}

fn window(floor: &str) -> Option<SyncWindow> {
    if floor == UNBOUNDED {
        Some(SyncWindow::full())
    } else {
        floor.parse::<CalendarDate>().ok().map(SyncWindow::since)
    }
}

/// An id on one line: a backslash and a line break written as two characters each.
fn escape(id: &str) -> String {
    id.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(escaped: &str) -> Option<String> {
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "stamp_tests.rs"]
mod tests;
