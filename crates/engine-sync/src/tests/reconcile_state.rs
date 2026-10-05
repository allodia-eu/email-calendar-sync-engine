//! A reconcile that carries state changes: what an IMAP session without QRESYNC sends, the flags
//! of everything it still holds beside the set it still holds. One pass has to move the flags
//! **and** tombstone what the set left out, or a message read or moved elsewhere stays unread,
//! or stays at all, until the next full download.

use engine_core::mail::{Keyword, SystemKeyword};

use super::*;

#[tokio::test]
async fn a_reconcile_moves_the_flags_it_carries_and_drops_what_it_left_out() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![
            message("m1", "a", "Kept and read elsewhere"),
            message("m2", "a", "Moved elsewhere"),
        ],
    )
    .then_changing_state(vec![MailStateChange::keywords(
        key("m1"),
        [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
    )])
    .then_reconciling(vec![key("m1")]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    for _ in 0..2 {
        sync_mail(
            core::slice::from_ref(&provider),
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await;
    }

    let rows = store
        .list_mail(
            &[account()],
            engine_store::MailSelector::Keys(&[key("m1"), key("m2")]),
            usize::MAX,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the message the set left out is gone");
    assert_eq!(rows[0].mail.key, key("m1"));
    assert!(
        rows[0].mail.flags.seen(),
        "the flag the change carried moved"
    );
    assert_eq!(
        rows[0].mail.subject.as_deref(),
        Some("Kept and read elsewhere")
    );
}
