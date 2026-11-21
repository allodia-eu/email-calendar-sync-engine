//! Offline tests for importing into IMAP, driven over a mock stream. Each asserts the bytes
//! sent as well as the answer, since the mock answers its script whatever it is sent; the live
//! suite (`tests/live_imap_import.rs`) is what shows a real server takes them.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
    mail::Keyword,
    membership::Memberships,
    raw::RawMime,
    sync::SyncState,
    time::UtcDateTime,
};
use engine_provider::{
    ProviderResult,
    import::{ImportedMessage, MailImporter, MessageImport, MessageMetadata},
};

use crate::{
    ImapProvider,
    mock::{MockStream, Recorded, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
const CAPABILITY: &str = "* CAPABILITY IMAP4rev1 UIDPLUS APPENDLIMIT=1000\r\na2 OK done\r\n";

/// A `SELECT "Archive"` answer under tag `tag`, keeping new keywords when `new_keywords`.
fn selected(tag: &str, new_keywords: bool) -> String {
    let star = if new_keywords { " \\*" } else { "" };
    format!(
        "* FLAGS (\\Seen \\Flagged)\r\n\
         * OK [PERMANENTFLAGS (\\Seen \\Flagged{star})] ok\r\n\
         * 2 EXISTS\r\n\
         * OK [UIDVALIDITY 7] ok\r\n\
         * OK [UIDNEXT 12] ok\r\n\
         {tag} OK [READ-WRITE] SELECT done\r\n"
    )
}

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn mailbox(id: &str) -> MailboxId {
    MailboxId::try_from(id).unwrap()
}

fn keywords(names: &[&str]) -> BTreeSet<Keyword> {
    names
        .iter()
        .map(|name| Keyword::new(*name).unwrap())
        .collect()
}

fn message(bytes: &[u8], folders: &[&str]) -> MessageImport {
    MessageImport::new(
        RawMime::new(bytes.to_vec()),
        Memberships::new(folders.iter().copied().map(mailbox)).unwrap(),
    )
}

fn with_keywords(message: MessageImport, names: &[&str]) -> MessageImport {
    message.with_metadata(MessageMetadata::default().with_keywords(keywords(names)))
}

/// A provider over a mock that has greeted, accepted login and negotiated `CAPABILITY`.
async fn provider(parts: &[&str]) -> (ImapProvider<MockStream>, Recorded) {
    provider_negotiating(&[CAPABILITY], parts).await
}

/// [`provider`], negotiating with `negotiation` (the `CAPABILITY` answer, and any `ENABLE`).
async fn provider_negotiating(
    negotiation: &[&str],
    parts: &[&str],
) -> (ImapProvider<MockStream>, Recorded) {
    let mut all = vec![GREETING, LOGIN_OK];
    all.extend_from_slice(negotiation);
    all.extend_from_slice(parts);
    let (stream, recorded) = MockStream::new(script(&all));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice", "pw").await.unwrap();
    connection.negotiate().await.unwrap();
    (
        ImapProvider::with_connection(connection, mailbox("INBOX")),
        recorded,
    )
}

fn class(answer: &ProviderResult<ImportedMessage>) -> FailureClass {
    answer.as_ref().unwrap_err().class()
}

#[tokio::test]
async fn each_message_is_appended_with_its_flags_keywords_and_arrival_time() {
    let select = selected("a3", true);
    let (provider, recorded) = provider(&[
        &select,
        "+ go\r\n",
        "a4 OK [APPENDUID 7 12] APPEND done\r\n",
        "+ go\r\n",
        "a5 OK APPEND done\r\n",
    ])
    .await;
    let received = "2019-02-03T04:05:06Z".parse::<UtcDateTime>().unwrap();
    let batch = [
        message(b"one", &["Archive"]).with_metadata(
            MessageMetadata::default()
                .with_keywords(keywords(&["$seen", "$flagged", "$label1"]))
                .with_received_at(received),
        ),
        message(b"second", &["Archive"]),
    ];

    let answers = provider.import_messages(&account(), &batch).await;

    let sent = written(&recorded);
    assert!(
        sent.contains(
            "a4 APPEND \"Archive\" (\\Flagged $label1 \\Seen) \"03-Feb-2019 04:05:06 +0000\" {3}\r\n"
        ),
        "{sent}"
    );
    assert!(sent.contains("a5 APPEND \"Archive\" () {6}\r\n"), "{sent}");
    assert_eq!(
        answers[0].as_ref().unwrap(),
        &ImportedMessage::created(Some(ProviderKey::new("imap:v7:u12@Archive").unwrap()))
            .with_kept(
                MessageMetadata::default()
                    .with_keywords(keywords(&["$seen", "$flagged", "$label1"]))
                    .with_received_at(received)
            )
    );
    // Without `APPENDUID` the target names no key; the message is found by its fingerprint.
    assert_eq!(
        answers[1].as_ref().unwrap(),
        &ImportedMessage::created(None)
    );
}

#[tokio::test]
async fn custom_keywords_are_left_out_where_the_folder_keeps_none() {
    let select = selected("a3", false);
    let (provider, recorded) = provider(&[&select, "+ go\r\n", "a4 OK APPEND done\r\n"]).await;
    let batch = [with_keywords(
        message(b"one", &["Archive"]),
        &["$seen", "$label1"],
    )];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(
        written(&recorded).contains("a4 APPEND \"Archive\" (\\Seen) {3}\r\n"),
        "{}",
        written(&recorded)
    );
    assert_eq!(
        answers[0].as_ref().unwrap().kept.keywords,
        keywords(&["$seen"]),
        "the receipt says the custom keyword was not stored"
    );
}

#[tokio::test]
async fn system_flags_alone_need_no_select() {
    let (provider, recorded) = provider(&["+ go\r\n", "a3 OK APPEND done\r\n"]).await;
    let batch = [with_keywords(message(b"one", &["Archive"]), &["$seen"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(
        !written(&recorded).contains("SELECT"),
        "{}",
        written(&recorded)
    );
    assert_eq!(
        answers[0].as_ref().unwrap().kept.keywords,
        keywords(&["$seen"])
    );
}

#[tokio::test]
async fn a_message_the_server_finds_too_big_is_refused_alone_and_the_batch_goes_on() {
    // A server may refuse the literal before it is sent, answering the tagged `NO` where the
    // continuation would be (RFC 7889 `TOOBIG`).
    let (provider, recorded) = provider(&[
        "a3 NO [TOOBIG] Message too large\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [message(b"big", &["INBOX"]), message(b"ok", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    let refusal = answers[0].as_ref().unwrap_err();
    assert!(refusal.is_item_unavailable(), "{refusal}");
    assert!(!refusal.requires_confirmation());
    assert!(answers[1].is_ok());
    assert!(
        !written(&recorded).contains("big"),
        "the refused literal never went out"
    );
}

#[tokio::test]
async fn a_message_over_the_stated_limit_or_in_two_mailboxes_never_reaches_the_server() {
    let (provider, recorded) = provider(&["+ go\r\n", "a3 OK APPEND done\r\n"]).await;
    let batch = [
        message(&[b'x'; 1001], &["INBOX"]),
        message(b"two", &["INBOX", "Archive"]),
        message(b"fits", &["INBOX"]),
    ];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(answers[0].as_ref().unwrap_err().is_item_unavailable());
    assert!(answers[1].as_ref().unwrap_err().is_item_unavailable());
    assert!(answers[2].is_ok());
    assert_eq!(
        written(&recorded).matches("APPEND").count(),
        1,
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_connection_lost_once_the_message_went_out_may_have_stored_it_and_ends_the_batch() {
    let (provider, _) = provider(&["+ go\r\n"]).await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(answers.len(), 2);
    assert!(answers[0].as_ref().unwrap_err().requires_confirmation());
    let unsent = answers[1].as_ref().unwrap_err();
    assert!(!unsent.requires_confirmation(), "{unsent}");
    assert_eq!(class(&answers[1]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_connection_lost_before_the_continuation_stored_nothing() {
    let (provider, _) = provider(&[]).await;
    let batch = [message(b"one", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    let failure = answers[0].as_ref().unwrap_err();
    assert!(!failure.requires_confirmation(), "{failure}");
    assert_eq!(class(&answers[0]), FailureClass::Retryable);
}

#[tokio::test]
async fn an_empty_batch_answers_nothing_and_sends_nothing() {
    let (provider, recorded) = provider(&[]).await;

    let answers = provider.import_messages(&account(), &[]).await;

    assert!(answers.is_empty());
    assert!(!written(&recorded).contains("APPEND"));
}

#[tokio::test]
async fn the_import_cursor_is_the_folder_as_a_delta_baseline() {
    let select = selected("a3", true);
    let (provider, recorded) = provider(&[&select]).await;

    let cursor = provider
        .import_cursor(&account(), &mailbox("Archive"))
        .await
        .unwrap();

    assert!(written(&recorded).contains("a3 SELECT \"Archive\""));
    assert_eq!(cursor, SyncState::new("v7;n12;g1"));
}

#[tokio::test]
async fn the_limits_are_one_mailbox_and_the_stated_appendlimit() {
    let (provider, _) = provider(&[]).await;

    let limits = provider.mail_import_limits(&account()).await.unwrap();

    assert_eq!(limits.max_mailboxes().get(), 1);
    assert_eq!(
        limits.max_message_size().map(std::num::NonZero::get),
        Some(1000)
    );
}

#[tokio::test]
async fn a_refusal_after_the_literal_stored_nothing_and_the_batch_goes_on() {
    let (provider, _) = provider(&[
        "+ go\r\n",
        "a3 NO [OVERQUOTA] Quota exceeded\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    let refusal = answers[0].as_ref().unwrap_err();
    assert!(!refusal.requires_confirmation(), "{refusal}");
    assert!(
        !refusal.is_item_unavailable(),
        "a full account is not about this message"
    );
    assert!(answers[1].is_ok());
}

#[tokio::test]
async fn a_bad_in_place_of_the_continuation_refuses_that_message_alone() {
    let (provider, _) = provider(&[
        "a3 BAD Invalid arguments\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert!(answers[1].is_ok());
}

#[tokio::test]
async fn an_unexpected_line_in_place_of_the_continuation_ends_the_batch_with_nothing_stored() {
    let (provider, _) = provider(&["hello there\r\n"]).await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert_eq!(class(&answers[1]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_folder_asked_about_once_answers_every_message_of_the_batch() {
    let select = selected("a3", true);
    let (provider, recorded) = provider(&[
        &select,
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
        "+ go\r\n",
        "a5 OK APPEND done\r\n",
    ])
    .await;
    let batch = [
        with_keywords(message(b"one", &["Archive"]), &["$label1"]),
        with_keywords(message(b"two", &["Archive"]), &["$label2"]),
    ];

    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(written(&recorded).matches("SELECT").count(), 1);
    assert_eq!(
        answers[1].as_ref().unwrap().kept.keywords,
        keywords(&["$label2"])
    );
}

#[tokio::test]
async fn a_folder_the_server_will_not_open_refuses_that_message_alone() {
    let (provider, _) = provider(&[
        "a3 NO Mailbox doesn't exist\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [
        with_keywords(message(b"one", &["Gone"]), &["$label1"]),
        message(b"two", &["INBOX"]),
    ];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert!(answers[1].is_ok());
}

#[tokio::test]
async fn a_connection_lost_while_opening_a_folder_ends_the_batch_with_nothing_stored() {
    let (provider, _) = provider(&[]).await;
    let batch = [
        with_keywords(message(b"one", &["Archive"]), &["$label1"]),
        message(b"two", &["INBOX"]),
    ];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert_eq!(class(&answers[1]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_batch_that_cannot_get_a_connection_is_answered_whole_by_that_failure() {
    // The first batch loses the only connection this test account has, on its last message,
    // and it cannot dial another; the next batch is answered message by message all the same.
    // Had the dead connection been parked, the next batch would have written to it.
    let (provider, recorded) = provider(&[]).await;
    let first = provider
        .import_messages(&account(), &[message(b"one", &["INBOX"])])
        .await;
    assert!(first[0].is_err());

    let answers = provider
        .import_messages(
            &account(),
            &[message(b"two", &["INBOX"]), message(b"three", &["INBOX"])],
        )
        .await;

    assert_eq!(answers.len(), 2);
    assert!(answers.iter().all(Result::is_err));
    assert_eq!(
        written(&recorded).matches("APPEND").count(),
        1,
        "the failed connection was discarded, not reused: {}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_server_that_states_no_appendlimit_states_no_largest_size() {
    let (provider, _) =
        provider_negotiating(&["* CAPABILITY IMAP4rev1\r\na2 OK done\r\n"], &[]).await;

    let limits = provider.mail_import_limits(&account()).await.unwrap();

    assert_eq!(limits.max_message_size(), None);
}

#[tokio::test]
async fn under_qresync_the_import_cursor_carries_the_highest_modseq() {
    let (provider, recorded) = provider_negotiating(
        &[
            "* CAPABILITY IMAP4rev1 QRESYNC\r\na2 OK done\r\n",
            "* ENABLED QRESYNC\r\na3 OK enabled\r\n",
        ],
        &["* 2 EXISTS\r\n\
           * OK [UIDVALIDITY 7] ok\r\n\
           * OK [UIDNEXT 12] ok\r\n\
           * OK [HIGHESTMODSEQ 99] ok\r\n\
           a4 OK [READ-WRITE] SELECT done\r\n"],
    )
    .await;

    let cursor = provider
        .import_cursor(&account(), &mailbox("Archive"))
        .await
        .unwrap();

    assert!(
        written(&recorded).contains("a4 SELECT \"Archive\" (CONDSTORE)"),
        "{}",
        written(&recorded)
    );
    assert_eq!(cursor, SyncState::new("v7;n12;g1;m99"));
}
