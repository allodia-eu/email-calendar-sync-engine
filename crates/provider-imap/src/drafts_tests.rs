//! Offline tests for storing and removing a draft, driven over a mock stream.

use engine_core::{
    error::FailureClass,
    ids::{MessageIdHeader, ProviderKey},
    mail::EmailAddress,
};
use engine_provider::Draft;

use super::{delete_draft, put_draft};
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

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
    let key = put_draft(&mut connection, &draft(), Some(&old))
        .await
        .unwrap();

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

    let key = put_draft(&mut connection, &draft(), None).await.unwrap();

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
    let key = put_draft(&mut connection, &draft(), Some(&old))
        .await
        .unwrap();

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
    delete_draft(&mut connection, &unplaced).await.unwrap();

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
    delete_draft(&mut connection, &unplaced).await.unwrap();
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
    let err = delete_draft(&mut connection, &stale).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
}
