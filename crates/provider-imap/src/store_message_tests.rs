//! Offline tests for the one routine that stores a message with its metadata: which keywords
//! ride the `APPEND`, when the folder is opened to find out, and what the answer says was kept.

use std::collections::BTreeSet;

use engine_core::mail::{Keyword, MessageMetadata, SystemKeyword};

use super::{FolderKeywords, store_message};
use crate::{
    error::ImapError,
    mock::{MockStream, Recorded, script, written},
    transport::Connection,
    transport_append::AppendFailure,
};

const SELECT_STAR: &str = "* 8 EXISTS\r\n* OK [UIDVALIDITY 4242] valid\r\n\
     * OK [PERMANENTFLAGS (\\Seen \\*)] ok\r\na1 OK [READ-WRITE] SELECT done\r\n";
const SELECT_NAMED: &str = "* 8 EXISTS\r\n* OK [UIDVALIDITY 4242] valid\r\n\
     * OK [PERMANENTFLAGS (\\Seen \\Flagged $Forwarded)] ok\r\na1 OK [READ-WRITE] SELECT done\r\n";

fn connection(parts: &[&str]) -> (Connection<MockStream>, Recorded) {
    let (stream, recorded) = MockStream::new(script(parts));
    (Connection::resume(stream), recorded)
}

fn keyword(name: &str) -> Keyword {
    Keyword::new(name).unwrap()
}

fn flagged() -> Keyword {
    Keyword::system(SystemKeyword::Flagged)
}

fn seen() -> Keyword {
    Keyword::system(SystemKeyword::Seen)
}

fn asking(keywords: &[Keyword]) -> MessageMetadata {
    MessageMetadata::default().with_keywords(keywords.iter().cloned().collect())
}

#[tokio::test]
async fn asking_for_no_keyword_opens_nothing_and_appends_the_role_flags_alone() {
    let (mut conn, recorded) =
        connection(&["+ go\r\n", "a1 OK [APPENDUID 4242 9] APPEND done\r\n"]);

    let stored = store_message(
        &mut conn,
        &mut FolderKeywords::default(),
        "Drafts",
        &["\\Draft", "\\Seen"],
        &MessageMetadata::default(),
        b"raw",
    )
    .await
    .unwrap();

    let sent = written(&recorded);
    assert!(!sent.contains("SELECT"), "{sent}");
    assert!(
        sent.contains("a1 APPEND \"Drafts\" (\\Draft \\Seen) {3}"),
        "{sent}"
    );
    assert_eq!(stored.append_uid, Some((4242, 9)));
    assert!(
        stored.kept.is_empty(),
        "a role flag is not something asked for"
    );
}

#[tokio::test]
async fn under_a_star_every_keyword_is_kept_but_only_the_system_flags_listed() {
    let (mut conn, recorded) = connection(&[
        SELECT_STAR,
        "+ go\r\n",
        "a2 OK [APPENDUID 4242 9] APPEND done\r\n",
    ]);

    let stored = store_message(
        &mut conn,
        &mut FolderKeywords::default(),
        "Archive",
        &[],
        &asking(&[seen(), flagged(), keyword("project-x")]),
        b"raw",
    )
    .await
    .unwrap();

    let sent = written(&recorded);
    assert!(
        sent.contains("a2 APPEND \"Archive\" (\\Seen project-x) {3}"),
        "{sent}"
    );
    assert_eq!(stored.kept, asking(&[seen(), keyword("project-x")]));
}

#[tokio::test]
async fn a_folder_that_names_its_keywords_keeps_those_it_names_without_a_star() {
    let (mut conn, recorded) = connection(&[
        SELECT_NAMED,
        "+ go\r\n",
        "a2 OK [APPENDUID 4242 9] APPEND done\r\n",
    ]);

    let stored = store_message(
        &mut conn,
        &mut FolderKeywords::default(),
        "Archive",
        &[],
        &asking(&[flagged(), keyword("$forwarded"), keyword("project-x")]),
        b"raw",
    )
    .await
    .unwrap();

    let sent = written(&recorded);
    assert!(
        sent.contains("a2 APPEND \"Archive\" (\\Flagged $forwarded) {3}"),
        "{sent}"
    );
    assert_eq!(stored.kept, asking(&[flagged(), keyword("$forwarded")]));
}

#[tokio::test]
async fn a_folder_that_will_not_open_takes_the_message_without_its_keywords() {
    let (mut conn, recorded) = connection(&[
        "a1 NO [NOPERM] no read right\r\n",
        "+ go\r\n",
        "a2 OK [APPENDUID 4242 9] APPEND done\r\n",
    ]);

    let stored = store_message(
        &mut conn,
        &mut FolderKeywords::default(),
        "Shared",
        &["\\Seen"],
        &asking(&[flagged(), keyword("project-x")]),
        b"raw",
    )
    .await
    .unwrap();

    let sent = written(&recorded);
    assert!(sent.contains("a2 APPEND \"Shared\" (\\Seen) {3}"), "{sent}");
    assert_eq!(stored.append_uid, Some((4242, 9)));
    assert!(stored.kept.is_empty());
}

#[tokio::test]
async fn a_select_that_gets_no_answer_stores_nothing() {
    let (mut conn, recorded) = connection(&[]);

    let failed = store_message(
        &mut conn,
        &mut FolderKeywords::default(),
        "Archive",
        &[],
        &asking(&[flagged()]),
        b"raw",
    )
    .await;

    let Err(AppendFailure::NotStored(lost)) = failed else {
        panic!("a lost SELECT stores nothing: {failed:?}");
    };
    assert!(
        !matches!(lost, ImapError::No(_) | ImapError::Bad(_)),
        "{lost:?}"
    );
    assert!(!written(&recorded).contains("APPEND"));
}

#[tokio::test]
async fn a_folder_is_opened_once_for_every_message_stored_in_it() {
    let (mut conn, recorded) = connection(&[
        SELECT_STAR,
        "+ go\r\n",
        "a2 OK [APPENDUID 4242 9] APPEND done\r\n",
        "+ go\r\n",
        "a3 OK [APPENDUID 4242 10] APPEND done\r\n",
    ]);
    let mut folders = FolderKeywords::default();

    for _ in 0..2 {
        store_message(
            &mut conn,
            &mut folders,
            "Archive",
            &[],
            &asking(&[keyword("project-x")]),
            b"raw",
        )
        .await
        .unwrap();
    }

    assert_eq!(written(&recorded).matches("SELECT").count(), 1);
}

#[tokio::test]
async fn the_arrival_time_goes_as_the_internaldate_in_whole_seconds_and_is_kept_so() {
    let (mut conn, recorded) =
        connection(&["+ go\r\n", "a1 OK [APPENDUID 4242 9] APPEND done\r\n"]);
    let asked =
        MessageMetadata::default().with_received_at("2019-02-03T04:05:06.5Z".parse().unwrap());

    let stored = store_message(
        &mut conn,
        &mut FolderKeywords::default(),
        "Archive",
        &[],
        &asked,
        b"raw",
    )
    .await
    .unwrap();

    let sent = written(&recorded);
    assert!(
        sent.contains("a1 APPEND \"Archive\" () \"03-Feb-2019 04:05:06 +0000\" {3}"),
        "{sent}"
    );
    assert_eq!(
        stored.kept.received_at,
        Some("2019-02-03T04:05:06Z".parse().unwrap())
    );
}

#[test]
fn the_flag_list_names_each_flag_once_whatever_its_case() {
    let keywords = BTreeSet::from([seen(), keyword("project-x")]);
    assert_eq!(
        super::flag_list(&["\\SEEN", "\\Draft"], &keywords),
        "\\SEEN \\Draft project-x"
    );
}
