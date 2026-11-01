//! Where an IMAP-read Gmail sync stands: each of the three folders' counters as the last pass
//! left them (`crate::imap_pass`).
//!
//! Written as `imap;a=<validity>.<uidnext>.<modseq>.<exists>;s=…;t=…`, with `-` for a folder the
//! account does not show to IMAP. The API pass's cursor is a bare history id or starts with a
//! word of its own, and neither reads the other's: a cursor from the other mode starts the mode
//! that finds it afresh, which costs a pass and loses nothing, the two writing the same keys.

use engine_core::sync::SyncState;
use provider_imap::{GmailFolder, GmailFolderState};

const PREFIX: &str = "imap;";

/// One folder's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FolderMark {
    pub(crate) uid_validity: u32,
    pub(crate) uid_next: u32,
    pub(crate) highest_modseq: u64,
    pub(crate) exists: u32,
}

impl From<GmailFolderState> for FolderMark {
    fn from(state: GmailFolderState) -> Self {
        Self {
            uid_validity: state.uid_validity,
            uid_next: state.uid_next,
            highest_modseq: state.highest_modseq,
            exists: state.exists,
        }
    }
}

/// The three folders' counters, in [`GmailFolder::ALL`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImapCursor(pub(crate) [Option<FolderMark>; 3]);

impl ImapCursor {
    /// The cursor `state` holds, or `None` for one this mode did not write.
    pub(crate) fn read(state: &SyncState) -> Option<Self> {
        let rest = state.as_str().strip_prefix(PREFIX)?;
        let mut marks = [None; 3];
        let mut seen = 0;
        for (part, (slot, letter)) in rest.split(';').zip(marks.iter_mut().zip(['a', 's', 't'])) {
            let value = part.strip_prefix(letter)?.strip_prefix('=')?;
            *slot = if value == "-" {
                None
            } else {
                Some(mark(value)?)
            };
            seen += 1;
        }
        (seen == 3).then_some(Self(marks))
    }

    pub(crate) fn state(&self) -> SyncState {
        let parts: Vec<String> = self
            .0
            .iter()
            .zip(['a', 's', 't'])
            .map(|(mark, letter)| match mark {
                Some(m) => format!(
                    "{letter}={}.{}.{}.{}",
                    m.uid_validity, m.uid_next, m.highest_modseq, m.exists
                ),
                None => format!("{letter}=-"),
            })
            .collect();
        SyncState::new(format!("{PREFIX}{}", parts.join(";")))
    }

    pub(crate) fn get(&self, folder: GmailFolder) -> Option<FolderMark> {
        self.0[index(folder)]
    }
}

pub(crate) fn index(folder: GmailFolder) -> usize {
    match folder {
        GmailFolder::All => 0,
        GmailFolder::Spam => 1,
        GmailFolder::Trash => 2,
    }
}

fn mark(value: &str) -> Option<FolderMark> {
    let mut fields = value.split('.');
    let mark = FolderMark {
        uid_validity: fields.next()?.parse().ok()?,
        uid_next: fields.next()?.parse().ok()?,
        highest_modseq: fields.next()?.parse().ok()?,
        exists: fields.next()?.parse().ok()?,
    };
    fields.next().is_none().then_some(mark)
}

#[cfg(test)]
#[path = "imap_cursor_tests.rs"]
mod tests;
