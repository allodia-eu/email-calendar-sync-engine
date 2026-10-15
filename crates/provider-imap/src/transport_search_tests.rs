//! A window search on a server that cuts a search off at its message limit, shaped like
//! Yahoo's: the newest matches by date, a plain `OK`, and nothing to say there were more.
//! The limit is 3 here so the cut is visible in a few lines.

use super::uid_spans;
use crate::{
    capability::Negotiated,
    mock::{MockStream, script, written},
    transport::Connection,
    transport_command::imap_day,
};

#[test]
fn spans_cover_the_uid_space_and_none_is_wider_than_the_limit() {
    assert_eq!(uid_spans(2501, 1000), ["1:1000", "1001:2000", "2001:2500"]);
    assert_eq!(uid_spans(2001, 1000), ["1:1000", "1001:2000"]);
    assert!(uid_spans(1, 1000).is_empty());
}

#[test]
fn a_day_is_read_from_a_search_date_and_from_an_internaldate() {
    let day = imap_day("1-Jul-2026").unwrap();
    assert_eq!(imap_day(" 1-Jul-2026 23:59:59 -1200"), Some(day));
    assert_eq!(imap_day("01-JUL-2026 00:00:00 +1400"), Some(day));
    assert_eq!(imap_day("31-Jun-2026"), None);
    assert_eq!(imap_day("garbage"), None);
}

async fn searched(capabilities: &[&str], replies: &[&str]) -> (Vec<u32>, String) {
    let mut server = vec![
        "* OK ready\r\n",
        "* 5 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\n* OK [UIDNEXT 6] n\r\na1 OK done\r\n",
    ];
    server.extend_from_slice(replies);
    let (stream, recorded) = MockStream::new(script(&server));
    let mut conn = Connection::open(stream).await.unwrap();
    let capabilities: Vec<String> = capabilities.iter().map(|&c| c.to_owned()).collect();
    conn.negotiated = Negotiated::from_capabilities(&capabilities);
    conn.select("INBOX").await.unwrap();
    let uids = conn.uid_search_since("1-Jul-2026").await.unwrap();
    (uids, written(&recorded))
}

fn dated(tag: &str, rows: &[(u32, &str)]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for (uid, date) in rows {
        write!(out, "* {uid} FETCH (UID {uid} INTERNALDATE \"{date}\")\r\n").unwrap();
    }
    write!(out, "{tag} OK done\r\n").unwrap();
    out
}

#[tokio::test]
async fn a_search_cut_off_at_the_limit_is_decided_by_each_messages_date() {
    // Four of five messages are in the window; the server returned the newest three.
    let first = dated(
        "a3",
        &[
            (1, "30-Jun-2026 23:00:00 +0000"),
            (2, " 1-Jul-2026 00:30:00 +0200"),
            (3, "15-Aug-2026 09:00:00 +0000"),
        ],
    );
    let second = dated(
        "a4",
        &[
            (4, "2-Sep-2026 09:00:00 +0000"),
            (5, "3-Sep-2026 09:00:00 +0000"),
        ],
    );
    let (uids, sent) = searched(
        &["MESSAGELIMIT=3"],
        &["* SEARCH 3 4 5\r\na2 OK done\r\n", &first, &second],
    )
    .await;

    assert_eq!(uids, [2, 3, 4, 5]);
    assert!(
        sent.contains("a3 UID FETCH 1:3 (UID INTERNALDATE)\r\n"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID FETCH 4:5 (UID INTERNALDATE)\r\n"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_search_under_the_limit_is_the_answer() {
    let (uids, sent) = searched(&["MESSAGELIMIT=3"], &["* SEARCH 4 5\r\na2 OK done\r\n"]).await;
    assert_eq!(uids, [4, 5]);
    assert!(!sent.contains("FETCH"), "{sent}");
}

#[tokio::test]
async fn a_server_stating_no_limit_is_taken_at_its_word() {
    let (uids, sent) = searched(&["IMAP4rev1"], &["* SEARCH 2 3 4 5\r\na2 OK done\r\n"]).await;
    assert_eq!(uids, [2, 3, 4, 5]);
    assert!(!sent.contains("FETCH"), "{sent}");
}

#[tokio::test]
async fn with_partial_the_dates_are_paged_by_message_count_not_by_uid() {
    // UIDs 1, 2, 4 and 5: the first page holds three messages across four UIDs, and the
    // second starts above the highest of them.
    let first = dated(
        "a3",
        &[
            (1, "30-Jun-2026 23:00:00 +0000"),
            (2, "1-Jul-2026 00:30:00 +0200"),
            (4, "2-Sep-2026 09:00:00 +0000"),
        ],
    );
    let second = dated("a4", &[(5, "3-Sep-2026 09:00:00 +0000")]);
    let (uids, sent) = searched(
        &["MESSAGELIMIT=3", "PARTIAL"],
        &["* SEARCH 2 4 5\r\na2 OK done\r\n", &first, &second],
    )
    .await;

    assert_eq!(uids, [2, 4, 5]);
    assert!(
        sent.contains("a3 UID FETCH 1:* (UID INTERNALDATE) (PARTIAL 1:3)\r\n"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID FETCH 5:* (UID INTERNALDATE) (PARTIAL 1:3)\r\n"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_full_last_page_ends_on_the_highest_message_named_again() {
    // `5:*` past the highest UID names UID 4 again, which ends the paging.
    let first = dated(
        "a3",
        &[
            (1, "1-Jul-2026 00:00:00 +0000"),
            (2, "1-Jul-2026 00:00:00 +0000"),
            (4, "1-Jul-2026 00:00:00 +0000"),
        ],
    );
    let second = dated("a4", &[(4, "1-Jul-2026 00:00:00 +0000")]);
    let (uids, _) = searched(
        &["MESSAGELIMIT=3", "PARTIAL"],
        &["* SEARCH 1 2 4\r\na2 OK done\r\n", &first, &second],
    )
    .await;

    assert_eq!(uids, [1, 2, 4]);
}

#[tokio::test]
async fn a_search_refused_for_its_size_is_decided_by_each_messages_date() {
    let only = dated("a3", &[(4, "2-Sep-2026 09:00:00 +0000")]);
    let (uids, _) = searched(
        &["MESSAGELIMIT=3", "PARTIAL"],
        &["a2 NO [LIMIT] UID SEARCH has partial results.\r\n", &only],
    )
    .await;
    assert_eq!(uids, [4]);
}

#[tokio::test]
async fn an_empty_window_is_not_mistaken_for_a_cut_off_one() {
    let (uids, sent) = searched(
        &["MESSAGELIMIT=3", "PARTIAL"],
        &["* SEARCH\r\na2 OK done\r\n"],
    )
    .await;
    assert!(uids.is_empty());
    assert!(!sent.contains("FETCH"), "{sent}");
}
