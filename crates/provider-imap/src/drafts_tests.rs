//! Offline tests for storing and removing a draft, driven over a mock stream.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword},
};
use engine_provider::Draft;

use super::{delete_draft, put_draft};
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

/// The credential's own store on a server advertising no namespaces — every folder is its
/// own, the behaviour these tests predate namespaces with.
fn own() -> crate::store::MailStore {
    crate::store::MailStore::own(&crate::store::Namespaces::default())
}

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
const DRAFTS_LIST: &str =
    "* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"\r\na2 OK LIST done\r\n";

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("compose-1@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

async fn connected(imap: Vec<u8>) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(imap);
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice", "pw").await.unwrap();
    (connection, recorded)
}

#[tokio::test]
async fn saving_again_appends_the_new_copy_then_removes_the_old() {
    let (mut connection, recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 70 9] APPEND completed\r\n",
        "* 4 EXISTS\r\na4 OK [UIDVALIDITY 70] SELECT completed\r\n",
        "a5 OK STORE completed\r\n",
        "a6 OK EXPUNGE completed\r\n",
    ]))
    .await;

    let old = ProviderKey::new("imap:v70:u4@Drafts").unwrap();
    let key = put_draft(
        &mut connection,
        &own(),
        &draft(),
        Some(&old),
        &BTreeSet::new(),
    )
    .await
    .unwrap()
    .key;

    // The key moves: IMAP messages are immutable, so the new version is a new message.
    assert_eq!(key.as_str(), "imap:v70:u9@Drafts");
    let sent = written(&recorded);
    assert!(
        sent.contains("APPEND \"Drafts\" (\\Draft \\Seen)"),
        "{sent}"
    );
    // And the superseded copy is gone, so the folder holds one draft rather than two.
    assert!(
        sent.contains("UID STORE 4 +FLAGS.SILENT (\\Deleted)"),
        "{sent}"
    );
    assert!(sent.contains("UID EXPUNGE 4"), "{sent}");
    // Ordering is the whole design: the new copy exists before the old one is touched.
    assert!(
        sent.find("APPEND").unwrap() < sent.find("UID STORE").unwrap(),
        "{sent}"
    );
}

#[tokio::test]
async fn a_first_save_appends_without_removing_anything() {
    let (mut connection, recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 70 9] APPEND completed\r\n",
    ]))
    .await;

    let key = put_draft(&mut connection, &own(), &draft(), None, &BTreeSet::new())
        .await
        .unwrap()
        .key;

    assert_eq!(key.as_str(), "imap:v70:u9@Drafts");
    assert!(!written(&recorded).contains("UID STORE"));
}

#[tokio::test]
async fn a_new_copy_that_cannot_replace_the_old_is_still_saved() {
    // The APPEND landed and the removal was rejected. Failing here would have the caller
    // retry the whole save, appending a third copy; the user gets a duplicate instead.
    let (mut connection, _recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 70 9] APPEND completed\r\n",
        "a4 NO [SERVERBUG] SELECT failed\r\n",
    ]))
    .await;

    let old = ProviderKey::new("imap:v70:u4@Drafts").unwrap();
    let key = put_draft(
        &mut connection,
        &own(),
        &draft(),
        Some(&old),
        &BTreeSet::new(),
    )
    .await
    .unwrap()
    .key;

    assert_eq!(key.as_str(), "imap:v70:u9@Drafts");
}

#[tokio::test]
async fn a_draft_saved_without_uidplus_is_found_by_its_message_id() {
    // No APPENDUID, so the key names the `Message-ID` we wrote; removing it has to
    // search the folder before it can address anything.
    let (mut connection, recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        "* 4 EXISTS\r\na3 OK [UIDVALIDITY 70] SELECT completed\r\n",
        "* SEARCH 4\r\na4 OK SEARCH completed\r\n",
        "a5 OK STORE completed\r\n",
        "a6 OK EXPUNGE completed\r\n",
    ]))
    .await;

    let unplaced = ProviderKey::new("draft:compose-1@test.local").unwrap();
    delete_draft(&mut connection, &own(), &unplaced)
        .await
        .unwrap();

    let sent = written(&recorded);
    assert!(
        sent.contains("UID SEARCH HEADER Message-ID \"compose-1@test.local\""),
        "{sent}"
    );
    assert!(
        sent.contains("UID STORE 4 +FLAGS.SILENT (\\Deleted)"),
        "{sent}"
    );
}

#[tokio::test]
async fn removing_a_draft_that_is_already_gone_is_done() {
    // The op is retryable, so the second delete must settle rather than park forever.
    let (mut connection, _recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        "* 0 EXISTS\r\na3 OK [UIDVALIDITY 70] SELECT completed\r\n",
        "* SEARCH\r\na4 OK SEARCH completed\r\n",
    ]))
    .await;

    let unplaced = ProviderKey::new("draft:compose-1@test.local").unwrap();
    delete_draft(&mut connection, &own(), &unplaced)
        .await
        .unwrap();
}

#[tokio::test]
async fn removing_a_draft_whose_folder_was_recreated_is_a_conflict() {
    // UIDVALIDITY moved, so UID 4 now names a different message. Deleting it would
    // delete someone else's mail, so the caller has to re-sync instead.
    let (mut connection, _recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        "* 4 EXISTS\r\na2 OK [UIDVALIDITY 71] SELECT completed\r\n",
    ]))
    .await;

    let stale = ProviderKey::new("imap:v70:u4@Drafts").unwrap();
    let err = delete_draft(&mut connection, &own(), &stale)
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
}

const DRAFTS_SELECT_ALLOWING_KEYWORDS: &str = "* OK [PERMANENTFLAGS (\\Deleted \\Seen \\Answered \\Flagged \\Draft \\*)] All allowed\r\n\
     * 0 EXISTS\r\na3 OK [UIDVALIDITY 70] SELECT completed\r\n";
const DRAFTS_SELECT_REFUSING_KEYWORDS: &str = "* OK [PERMANENTFLAGS (\\Deleted \\Seen \\Answered \\Flagged \\Draft)] Limited\r\n\
     * 0 EXISTS\r\na3 OK [UIDVALIDITY 70] SELECT completed\r\n";

fn project_x() -> BTreeSet<Keyword> {
    BTreeSet::from([Keyword::new("project-x").unwrap()])
}

#[tokio::test]
async fn a_draft_s_keywords_ride_its_append_where_the_folder_keeps_them() {
    let (mut connection, recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        DRAFTS_SELECT_ALLOWING_KEYWORDS,
        "+ OK send literal\r\n",
        "a4 OK [APPENDUID 70 9] APPEND completed\r\n",
    ]))
    .await;

    let receipt = put_draft(&mut connection, &own(), &draft(), None, &project_x())
        .await
        .unwrap();

    assert_eq!(receipt.key.as_str(), "imap:v70:u9@Drafts");
    assert_eq!(receipt.keywords, project_x());
    let sent = written(&recorded);
    assert!(
        sent.contains("APPEND \"Drafts\" (\\Draft \\Seen project-x)"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_folder_that_allows_no_new_keyword_gets_the_draft_without_them() {
    // Without `\*` the server would accept the flag with a plain OK and keep nothing, so it
    // is not asked for, and the receipt says it was not kept. The draft is still saved.
    let (mut connection, recorded) = connected(script(&[
        GREETING,
        LOGIN_OK,
        DRAFTS_LIST,
        DRAFTS_SELECT_REFUSING_KEYWORDS,
        "+ OK send literal\r\n",
        "a4 OK [APPENDUID 70 9] APPEND completed\r\n",
    ]))
    .await;

    let receipt = put_draft(&mut connection, &own(), &draft(), None, &project_x())
        .await
        .unwrap();

    assert_eq!(receipt.key.as_str(), "imap:v70:u9@Drafts");
    assert_eq!(receipt.keywords, BTreeSet::new());
    assert!(
        written(&recorded).contains("APPEND \"Drafts\" (\\Draft \\Seen)"),
        "{}",
        written(&recorded)
    );
}
