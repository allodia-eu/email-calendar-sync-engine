//! Offline tests for the Gmail reads, over a scripted stream with Gmail's response shapes.

use engine_core::{error::FailureClass, ids::MailboxId};

use super::*;
use crate::mock::{MockStream, Recorded, script, written};

const GREETING: &str = "* OK Gimap ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

async fn logged_in(replies: &[&str]) -> (Connection<MockStream>, Recorded) {
    let mut parts = vec![GREETING, LOGIN_OK];
    parts.extend_from_slice(replies);
    let (stream, recorded) = MockStream::new(script(&parts));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice@example.com", "pw").await.unwrap();
    (connection, recorded)
}

fn all_mail() -> MailboxId {
    MailboxId::try_from("[Gmail]/All Mail").unwrap()
}

const SELECT_ALL: &str = concat!(
    "* 855 EXISTS\r\n",
    "* OK [UIDVALIDITY 11] UIDs valid.\r\n",
    "* OK [UIDNEXT 6871] Predicted next UID.\r\n",
    "* OK [HIGHESTMODSEQ 234042]\r\n",
    "a2 OK [READ-WRITE] [Gmail]/All Mail selected. (Success)\r\n",
);

#[tokio::test]
async fn open_reports_the_counters_a_pass_plans_from() {
    let (mut connection, recorded) = logged_in(&[SELECT_ALL]).await;
    let state = open(&mut connection, "[Gmail]/All Mail").await.unwrap();
    assert_eq!(
        state,
        GmailFolderState {
            uid_validity: 11,
            uid_next: 6871,
            highest_modseq: 234_042,
            exists: 855
        }
    );
    assert!(written(&recorded).contains("a2 SELECT \"[Gmail]/All Mail\" (CONDSTORE)"));
}

#[tokio::test]
async fn a_renumbered_folder_asks_for_a_fresh_sync_rather_than_misfiling() {
    let (mut connection, _) = logged_in(&[SELECT_ALL]).await;
    let err = ensure_open(&mut connection, "[Gmail]/All Mail", 10)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::NeedsResync);
}

#[tokio::test]
async fn an_open_folder_at_the_same_validity_is_not_opened_again() {
    let (mut connection, recorded) = logged_in(&[SELECT_ALL, "* SEARCH 4\r\na3 OK done\r\n"]).await;
    open(&mut connection, "[Gmail]/All Mail").await.unwrap();
    ensure_open(&mut connection, "[Gmail]/All Mail", 11)
        .await
        .unwrap();
    search(&mut connection, &GmailQuery::default())
        .await
        .unwrap();
    let sent = written(&recorded);
    assert_eq!(sent.matches("SELECT").count(), 1, "{sent}");
    assert!(sent.contains("a3 UID SEARCH ALL"), "{sent}");
}

#[tokio::test]
async fn a_search_joins_every_criterion_it_was_given() {
    let (mut connection, recorded) =
        logged_in(&["* SEARCH 7 3\r\na2 OK SEARCH completed\r\n"]).await;
    let query = GmailQuery {
        uids: Some(vec![3, 1, 2, 7]),
        since: time::Date::from_calendar_date(2026, time::Month::October, 1).ok(),
        raw: Some("category:social".to_owned()),
        ..GmailQuery::default()
    };
    assert_eq!(search(&mut connection, &query).await.unwrap(), vec![3, 7]);
    assert!(
        written(&recorded)
            .contains(r#"a2 UID SEARCH UID 1:3,7 SINCE 1-Oct-2026 X-GM-RAW "category:social""#),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_search_within_no_uids_asks_the_server_nothing() {
    let (mut connection, recorded) = logged_in(&[]).await;
    let query = GmailQuery {
        uids: Some(Vec::new()),
        ..GmailQuery::default()
    };
    assert_eq!(
        search(&mut connection, &query).await.unwrap(),
        Vec::<u32>::new()
    );
    assert!(!written(&recorded).contains("SEARCH"));
}

#[tokio::test]
async fn metadata_carries_the_gmail_ids_and_the_labels_decoded() {
    let headers = "Content-Type: text/plain\r\n\r\n";
    let row = format!(
        concat!(
            "* 12 FETCH (X-GM-THRID 1402140321003522132 X-GM-MSGID 1402140321003522133 ",
            r#"X-GM-LABELS ("\\Inbox" "Reis/&AOk-t&AOk-") UID 5002 FLAGS (\Seen) "#,
            r#"INTERNALDATE "01-Oct-2026 10:00:00 +0000" RFC822.SIZE 420 "#,
            r#"ENVELOPE ("Thu, 1 Oct 2026 10:00:00 +0000" "Hi" NIL NIL NIL NIL NIL NIL NIL "<m@example.com>") "#,
            "BODY[HEADER.FIELDS (REFERENCES CONTENT-TYPE CONTENT-DISPOSITION)] {{{}}}\r\n{})\r\n",
            "a2 OK Success\r\n"
        ),
        headers.len(),
        headers
    );
    let (mut connection, recorded) = logged_in(&[&row]).await;
    let got = messages(&mut connection, &all_mail(), 11, &[5002])
        .await
        .unwrap();
    let sent = written(&recorded);
    assert!(
        sent.contains("X-GM-MSGID X-GM-THRID X-GM-LABELS)"),
        "{sent}"
    );
    assert_eq!(got.len(), 1);
    let state = &got[0].state;
    assert_eq!(
        (state.uid, state.gmail_id, state.thread_id),
        (5002, 1_402_140_321_003_522_133, 1_402_140_321_003_522_132)
    );
    assert_eq!(
        state.labels,
        vec!["\\Inbox".to_owned(), "Reis/été".to_owned()]
    );
    assert_eq!(state.flags, vec!["\\Seen".to_owned()]);
    assert_eq!(got[0].message.envelope.subject.as_deref(), Some("Hi"));
    assert_eq!(got[0].message.size, Some(420));
    assert_eq!(
        got[0].date.as_deref(),
        Some("Thu, 1 Oct 2026 10:00:00 +0000")
    );
}

#[tokio::test]
async fn changes_ask_condstore_alone_and_drop_the_arrivals_above_the_line() {
    let reply = concat!(
        r#"* 3 FETCH (UID 40 FLAGS () X-GM-MSGID 9 X-GM-THRID 9 X-GM-LABELS ("\\Inbox") MODSEQ (51))"#,
        "\r\n",
        r#"* 4 FETCH (UID 99 FLAGS () X-GM-MSGID 10 X-GM-THRID 10 X-GM-LABELS () MODSEQ (52))"#,
        "\r\n",
        "a2 OK Success\r\n",
    );
    let (mut connection, recorded) = logged_in(&[reply]).await;
    let got = changed(&mut connection, 50, 50).await.unwrap();
    let sent = written(&recorded);
    assert!(
        sent.contains(
            "a2 UID FETCH 1:49 (UID FLAGS X-GM-MSGID X-GM-THRID X-GM-LABELS) (CHANGEDSINCE 50)\r\n"
        ),
        "{sent}"
    );
    assert!(!sent.contains("VANISHED"), "{sent}");
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].uid, got[0].gmail_id, got[0].modseq),
        (40, 9, Some(51))
    );
}

#[tokio::test]
async fn messages_are_located_by_gmail_id() {
    let replies = [
        "* SEARCH 8 5\r\na2 OK done\r\n",
        "* 1 FETCH (UID 5 X-GM-MSGID 300)\r\n* 2 FETCH (UID 8 X-GM-MSGID 100)\r\na3 OK done\r\n",
    ];
    let (mut connection, recorded) = logged_in(&replies).await;
    let got = located(&mut connection, &[100, 200, 300]).await.unwrap();
    let sent = written(&recorded);
    assert!(
        sent.contains("a2 UID SEARCH OR OR X-GM-MSGID 100 X-GM-MSGID 200 X-GM-MSGID 300\r\n"),
        "{sent}"
    );
    assert!(sent.contains("a3 UID FETCH 5,8 (UID X-GM-MSGID)"), "{sent}");
    assert_eq!(got, vec![(5, 300), (8, 100)]);
}

#[tokio::test]
async fn arrivals_are_asked_for_as_a_range_and_the_highest_uid_answer_is_dropped() {
    // `UID 50:*` with every UID below 50 still names the highest one (RFC 9051 §6.4.8).
    let (mut connection, recorded) = logged_in(&["* SEARCH 49\r\na2 OK done\r\n"]).await;
    let query = GmailQuery {
        from_uid: Some(50),
        ..GmailQuery::default()
    };
    assert_eq!(
        search(&mut connection, &query).await.unwrap(),
        Vec::<u32>::new()
    );
    assert!(written(&recorded).contains("a2 UID SEARCH UID 50:*\r\n"));
}

#[tokio::test]
async fn a_long_uid_list_is_asked_about_a_thousand_at_a_time() {
    let uids: Vec<u32> = (1..=2500).map(|n| n * 2).collect();
    let replies = [
        "* SEARCH 2\r\na2 OK done\r\n",
        "* SEARCH 2002\r\na3 OK done\r\n",
        "* SEARCH 4002\r\na4 OK done\r\n",
    ];
    let (mut connection, recorded) = logged_in(&replies).await;
    let query = GmailQuery {
        uids: Some(uids),
        ..GmailQuery::default()
    };
    assert_eq!(
        search(&mut connection, &query).await.unwrap(),
        vec![2, 2002, 4002]
    );
    let sent = written(&recorded);
    assert_eq!(sent.matches("UID SEARCH").count(), 3, "{sent}");
    assert!(
        sent.lines().all(|line| line.len() < 8192),
        "every command stays under RFC 7162's 8192 octets"
    );
}

#[test]
fn a_single_id_is_searched_without_an_or() {
    assert_eq!(any_of(&[7]), "X-GM-MSGID 7");
}

#[test]
fn batches_read_the_newest_uids_first() {
    assert_eq!(batches(&[1, 2, 3, 9, 10], 2), vec!["9:10", "2:3", "1"]);
    assert_eq!(batches(&[4, 5], 0), vec!["4:5"]);
}
