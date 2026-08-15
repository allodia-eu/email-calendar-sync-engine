//! Offline tests for the opaque page token: the snapshot offset and delta state a paused
//! member pass resumes from, and the errors a malformed one must produce rather than panic.

use super::*;

#[test]
fn page_cursor_round_trips_through_its_opaque_token() {
    // Snapshot offsets and delta states survive encode → decode unchanged.
    let snap = PageCursor::Snapshot(42).to_token();
    assert_eq!(snap.as_str(), "s:42");
    assert!(matches!(
        PageCursor::parse(&snap).unwrap(),
        PageCursor::Snapshot(42)
    ));

    let delta = PageCursor::Delta(SyncState::new("changes-state")).to_token();
    assert_eq!(delta.as_str(), "d:changes-state");
    match PageCursor::parse(&delta).unwrap() {
        PageCursor::Delta(state) => assert_eq!(state.as_str(), "changes-state"),
        PageCursor::Snapshot(_) => panic!("expected a delta cursor"),
    }
}

#[test]
fn malformed_page_tokens_are_protocol_errors_not_panics() {
    // A non-numeric snapshot offset and an unknown prefix both error cleanly.
    assert!(PageCursor::parse(&PageToken::new("s:not-a-number")).is_err());
    assert!(PageCursor::parse(&PageToken::new("garbage")).is_err());
}
