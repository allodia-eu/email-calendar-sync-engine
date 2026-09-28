//! Offline tests for the keywords a placement asks for on the sender's filed copy: asked for
//! only where the folder allows new keywords, reported as kept only then, and set on a copy
//! an earlier attempt already placed.

use std::collections::BTreeSet;

use engine_core::{ids::MessageIdHeader, mail::Keyword};

use super::{Filing, append_to_role_folder, place_if_absent};
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN completed\r\n";
const SENT_LIST: &str = "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n";
const SELECT_ALLOWS_NEW: &str = "* 8 EXISTS\r\n* OK [UIDVALIDITY 4242] valid\r\n\
     * OK [PERMANENTFLAGS (\\Seen \\Flagged \\*)] ok\r\na3 OK [READ-WRITE] SELECT done\r\n";
const SELECT_FIXED_FLAGS: &str = "* 8 EXISTS\r\n* OK [UIDVALIDITY 4242] valid\r\n\
     * OK [PERMANENTFLAGS (\\Seen \\Flagged)] ok\r\na3 OK [READ-WRITE] SELECT done\r\n";

async fn connection(parts: &[&str]) -> (Connection<MockStream>, crate::mock::Recorded) {
    let mut all = vec![GREETING, LOGIN_OK];
    all.extend_from_slice(parts);
    let (stream, recorded) = MockStream::new(script(&all));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

fn keywords() -> BTreeSet<Keyword> {
    BTreeSet::from([Keyword::new("project-x").unwrap()])
}

fn own() -> crate::store::MailStore {
    crate::store::MailStore::own(&crate::store::Namespaces::default())
}

fn message_id() -> MessageIdHeader {
    MessageIdHeader::new("placed-probe@test.local").unwrap()
}

#[tokio::test]
async fn keywords_ride_the_append_where_the_folder_allows_new_ones() {
    let (mut conn, recorded) = connection(&[
        SENT_LIST,
        SELECT_ALLOWS_NEW,
        "+ OK literal\r\n",
        "a4 OK [APPENDUID 4242 9] APPEND completed\r\n",
    ])
    .await;

    let placed = append_to_role_folder(&mut conn, &own(), Filing::Sent, b"raw", &keywords())
        .await
        .unwrap();

    assert_eq!(placed.keywords, keywords());
    assert_eq!(placed.append_uid, Some((4242, 9)));
    let sent = written(&recorded);
    assert!(
        sent.contains("APPEND \"Sent\" (\\Seen project-x) {3}"),
        "{sent}"
    );
}

/// A folder without `\*` in `PERMANENTFLAGS` answers the `APPEND` with `OK` and keeps no new
/// keyword, so the copy is placed without them and the placement says none were kept.
#[tokio::test]
async fn a_folder_that_allows_no_new_keywords_gets_the_copy_without_them() {
    let (mut conn, recorded) = connection(&[
        SENT_LIST,
        SELECT_FIXED_FLAGS,
        "+ OK literal\r\n",
        "a4 OK [APPENDUID 4242 9] APPEND completed\r\n",
    ])
    .await;

    let placed = append_to_role_folder(&mut conn, &own(), Filing::Sent, b"raw", &keywords())
        .await
        .unwrap();

    assert!(placed.keywords.is_empty());
    let sent = written(&recorded);
    assert!(sent.contains("APPEND \"Sent\" (\\Seen) {3}"), "{sent}");
}

/// Asking nothing costs nothing: the first attempt of an untagged send issues no `SELECT`.
#[tokio::test]
async fn a_placement_without_keywords_selects_nothing() {
    let (mut conn, recorded) = connection(&[
        SENT_LIST,
        "+ OK literal\r\n",
        "a3 OK [APPENDUID 4242 9] APPEND completed\r\n",
    ])
    .await;

    let placed = append_to_role_folder(&mut conn, &own(), Filing::Sent, b"raw", &BTreeSet::new())
        .await
        .unwrap();

    assert!(placed.keywords.is_empty());
    assert!(!written(&recorded).contains("SELECT"));
}

/// A retry that finds the copy an earlier attempt placed cannot know whether that attempt
/// set the keywords, so it sets them; `+FLAGS` is idempotent.
#[tokio::test]
async fn a_copy_already_placed_is_given_the_keywords() {
    let (mut conn, recorded) = connection(&[
        SENT_LIST,
        SELECT_ALLOWS_NEW,
        "* SEARCH 17\r\na4 OK SEARCH completed\r\n",
        "a5 OK STORE completed\r\n",
    ])
    .await;

    let placed = place_if_absent(
        &mut conn,
        &own(),
        Filing::Sent,
        &message_id(),
        b"raw",
        &keywords(),
    )
    .await
    .unwrap();

    assert_eq!(placed.append_uid, Some((4242, 17)));
    assert_eq!(placed.keywords, keywords());
    let sent = written(&recorded);
    assert!(
        sent.contains("UID STORE 17 +FLAGS.SILENT (project-x)"),
        "{sent}"
    );
    assert!(!sent.contains("APPEND"), "{sent}");
}

#[tokio::test]
async fn a_retry_appends_with_the_keywords_when_no_copy_is_there() {
    let (mut conn, recorded) = connection(&[
        SENT_LIST,
        SELECT_ALLOWS_NEW,
        "* SEARCH\r\na4 OK SEARCH completed\r\n",
        "+ OK literal\r\n",
        "a5 OK [APPENDUID 4242 18] APPEND completed\r\n",
    ])
    .await;

    let placed = place_if_absent(
        &mut conn,
        &own(),
        Filing::Sent,
        &message_id(),
        b"raw",
        &keywords(),
    )
    .await
    .unwrap();

    assert_eq!(placed.keywords, keywords());
    assert!(written(&recorded).contains("APPEND \"Sent\" (\\Seen project-x) {3}"));
}
