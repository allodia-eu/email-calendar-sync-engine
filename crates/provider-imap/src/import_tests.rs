//! Offline tests for importing into IMAP, driven over a mock stream. Each asserts the bytes
//! sent as well as the answer, since the mock answers its script whatever it is sent; the live
//! suite (`tests/live_imap_import.rs`) is what shows a real server takes them.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Keyword, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    sync::SyncState,
    time::UtcDateTime,
};
use engine_provider::{
    ProviderResult,
    import::{ImportedMessage, MailImporter, MessageImport},
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
async fn the_receipt_reports_the_arrival_time_in_the_whole_seconds_stored() {
    let (provider, _) = provider(&["+ go\r\n", "a3 OK APPEND done\r\n"]).await;
    let at = "2019-02-03T04:05:06.5Z".parse::<UtcDateTime>().unwrap();
    let batch =
        [message(b"one", &["INBOX"])
            .with_metadata(MessageMetadata::default().with_received_at(at))];

    let answers = provider.import_messages(&account(), &batch).await;

    let kept = answers[0].as_ref().unwrap().kept.received_at;
    assert_eq!(kept, Some("2019-02-03T04:05:06Z".parse().unwrap()));
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
async fn a_message_asking_for_no_keyword_opens_no_folder() {
    let (provider, recorded) = provider(&["+ go\r\n", "a3 OK APPEND done\r\n"]).await;
    let batch = [message(b"one", &["Archive"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(
        !written(&recorded).contains("SELECT"),
        "{}",
        written(&recorded)
    );
    assert!(answers[0].as_ref().unwrap().kept.is_empty());
}

/// A system flag is kept only where the folder lists it, `\*` or not: the rule every IMAP
/// write that sets a keyword follows (`crate::permanent`).
#[tokio::test]
async fn a_system_flag_the_folder_does_not_list_is_left_out() {
    let select = "* OK [PERMANENTFLAGS (\\Seen \\*)] ok\r\n\
         * OK [UIDVALIDITY 7] ok\r\na3 OK [READ-WRITE] SELECT done\r\n";
    let (provider, recorded) = provider(&[select, "+ go\r\n", "a4 OK APPEND done\r\n"]).await;
    let batch = [with_keywords(
        message(b"one", &["Archive"]),
        &["$seen", "$flagged", "$label1"],
    )];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(
        written(&recorded).contains("a4 APPEND \"Archive\" ($label1 \\Seen) {3}\r\n"),
        "{}",
        written(&recorded)
    );
    assert_eq!(
        answers[0].as_ref().unwrap().kept.keywords,
        keywords(&["$seen", "$label1"])
    );
}

/// A folder that names its keywords keeps those it names, with no `\*`.
#[tokio::test]
async fn a_folder_that_names_a_keyword_keeps_it_without_a_star() {
    let select = "* OK [PERMANENTFLAGS (\\Seen $label1)] ok\r\n\
         * OK [UIDVALIDITY 7] ok\r\na3 OK [READ-WRITE] SELECT done\r\n";
    let (provider, _) = provider(&[select, "+ go\r\n", "a4 OK APPEND done\r\n"]).await;
    let batch = [with_keywords(
        message(b"one", &["Archive"]),
        &["$label1", "$label2"],
    )];

    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(
        answers[0].as_ref().unwrap().kept.keywords,
        keywords(&["$label1"])
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

/// A folder the server will not open (no read right, say) still takes the message: keywords are
/// never worth the message, so it is stored without them and the receipt says none were kept.
#[tokio::test]
async fn a_folder_the_server_will_not_open_takes_the_message_without_its_keywords() {
    let (provider, recorded) = provider(&[
        "a3 NO [NOPERM] no read right\r\n",
        "+ go\r\n",
        "a4 OK [APPENDUID 7 12] APPEND done\r\n",
    ])
    .await;
    let batch = [with_keywords(message(b"one", &["Shared"]), &["$label1"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(
        written(&recorded).contains("a4 APPEND \"Shared\" () {3}\r\n"),
        "{}",
        written(&recorded)
    );
    let stored = answers[0].as_ref().unwrap();
    assert_eq!(
        stored.key,
        Some(ProviderKey::new("imap:v7:u12@Shared").unwrap())
    );
    assert!(stored.kept.is_empty());
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

fn importer_of<P: engine_provider::Provider + ?Sized>(provider: &P) -> Option<&dyn MailImporter> {
    provider.mail_importer()
}

/// A host holding the adapter as a `Box<dyn Provider>` imports through the accessor, without
/// knowing it is IMAP.
#[tokio::test]
async fn a_boxed_imap_provider_reaches_its_importer_through_the_accessor() {
    let (provider, recorded) = provider(&["+ go\r\n", "a3 OK APPEND done\r\n"]).await;
    let held: Box<dyn engine_provider::Provider> = Box::new(provider);

    let importer = importer_of(&held).expect("IMAP imports");
    let answers = importer
        .import_messages(&account(), &[message(b"one", &["INBOX"])])
        .await;

    assert!(answers[0].is_ok(), "{answers:?}");
    assert!(written(&recorded).contains("a3 APPEND \"INBOX\" () {3}\r\n"));
}

#[path = "import_failure_tests.rs"]
mod failures;
