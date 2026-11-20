//! The depth an email cursor was fetched under, stored with it.
//!
//! A cursor only goes on from what its pass fetched, so one written under a later floor than a
//! pass asks for knows nothing of the mail between the two floors, and that pass starts the
//! scope over. A host clears the cursors when it widens the window, but a pass that began under
//! the old window can still claim a folder after the clear and write a cursor for it; this is
//! what keeps that cursor from standing.
//!
//! The depth is the scope's own: a folder-bound scope (IMAP, Graph) records its folder's floor,
//! and an account-wide scope (JMAP, Gmail) records the account's floor and the floor of every
//! mailbox held further back ([`MailboxWindows::deepened`]). So deepening one mailbox starts
//! over the one scope that fetches it, with no cursor cleared by the host.
//!
//! A pass asking for a **later** floor resumes, as does one that no longer deepens a mailbox: a
//! host computes its floor from today, so the floor moves on every day with nothing new to
//! fetch. Mail a narrower window no longer wants is left to the delta filter
//! (`MailboxWindows::admits`) and to `prune_account_mail_outside_window`.
//!
//! A pass over the full history writes the provider's cursor bare, so a bare cursor reads as
//! one fetched under no floor at all, which is also how every cursor written before floors were
//! recorded reads.

use engine_core::{
    ids::MailboxId,
    sync::{MailboxWindows, SyncState, SyncWindow},
    time::CalendarDate,
};

const SINCE: &str = "since=";
const DEEPER: &str = "deeper=";
/// The floor of a mailbox held over its whole history.
const UNBOUNDED: &str = "*";
const END: char = '\n';

/// The provider's cursor to resume `depth` from, or `None` when `stored` was fetched under a
/// later floor, for the account or for a mailbox `depth` deepens, and the scope starts over.
pub(crate) fn resume(stored: Option<SyncState>, depth: &MailboxWindows) -> Option<SyncState> {
    let (fetched, cursor) = split(stored?);
    let covered = covers(depth.account(), fetched.account())
        && depth
            .deepened()
            .all(|(mailbox, window)| covers(window, fetched.window_for(mailbox)));
    covered.then_some(cursor)
}

/// `cursor` as stored by a pass over `depth`.
pub(crate) fn stamp(cursor: SyncState, depth: &MailboxWindows) -> SyncState {
    use core::fmt::Write as _;
    let Some(floor) = depth.account().floor() else {
        return cursor;
    };
    let mut stamped = format!("{SINCE}{floor}{END}");
    for (mailbox, window) in depth.deepened() {
        let floor = window
            .floor()
            .map_or_else(|| UNBOUNDED.to_owned(), |floor| floor.to_string());
        let _ = write!(stamped, "{DEEPER}{floor} {}{END}", escape(mailbox.as_str()));
    }
    stamped.push_str(cursor.as_str());
    SyncState::new(stamped)
}

/// Whether a cursor fetched under `fetched` holds everything `asked` admits.
fn covers(asked: SyncWindow, fetched: SyncWindow) -> bool {
    match (asked.floor(), fetched.floor()) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(asked), Some(fetched)) => asked >= fetched,
    }
}

/// The depth a stored cursor was fetched under and the provider's cursor. A cursor that only
/// looks stamped is the provider's own, whole, and reads as fetched under the full history.
fn split(stored: SyncState) -> (MailboxWindows, SyncState) {
    match parse(stored.as_str()) {
        Some((depth, cursor)) => (depth, SyncState::new(cursor)),
        None => (MailboxWindows::default(), stored),
    }
}

fn parse(raw: &str) -> Option<(MailboxWindows, &str)> {
    let (floor, mut rest) = raw.strip_prefix(SINCE)?.split_once(END)?;
    let mut depth = MailboxWindows::new(SyncWindow::since(floor.parse().ok()?));
    while let Some(line) = rest.strip_prefix(DEEPER) {
        let (entry, after) = line.split_once(END)?;
        let (floor, mailbox) = entry.split_once(' ')?;
        let window = if floor == UNBOUNDED {
            SyncWindow::full()
        } else {
            SyncWindow::since(floor.parse::<CalendarDate>().ok()?)
        };
        let mailbox = MailboxId::try_from(unescape(mailbox)?.as_str()).ok()?;
        depth = depth.deepen(mailbox, window);
        rest = after;
    }
    Some((depth, rest))
}

/// A mailbox id on one line: a backslash and a line break written as two characters each.
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
#[path = "window_cursor_tests.rs"]
mod tests;
