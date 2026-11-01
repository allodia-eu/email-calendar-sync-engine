use engine_core::sync::SyncState;
use provider_imap::GmailFolder;

use super::*;

fn mark(validity: u32) -> FolderMark {
    FolderMark {
        uid_validity: validity,
        uid_next: 6871,
        highest_modseq: 234_042,
        exists: 855,
    }
}

#[test]
fn a_cursor_reads_back_as_it_was_written() {
    let cursor = ImapCursor([Some(mark(11)), None, Some(mark(13))]);
    let state = cursor.state();
    assert_eq!(
        state.as_str(),
        "imap;a=11.6871.234042.855;s=-;t=13.6871.234042.855"
    );
    assert!(
        !state.as_str().contains('\n'),
        "the engine prefixes a window line"
    );
    assert_eq!(ImapCursor::read(&state), Some(cursor));
    assert_eq!(cursor.get(GmailFolder::Spam), None);
    assert_eq!(cursor.get(GmailFolder::Trash), Some(mark(13)));
}

#[test]
fn the_api_passes_cursors_are_not_this_modes() {
    for raw in [
        "234042",
        "backfill;h=1;p=",
        "reconcile;h=1",
        "imap;a=1.2.3.4;s=-",
        "imap;a=1.2.3;s=-;t=-",
        "imap;s=-;a=-;t=-",
    ] {
        assert_eq!(ImapCursor::read(&SyncState::new(raw)), None, "{raw}");
    }
}
