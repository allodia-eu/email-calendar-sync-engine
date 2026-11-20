//! Offline tests for [`GmailMail`] over an account's pool: finding the three folders, every
//! read through a borrowed connection, and the refusals a caller relies on.
//!
//! Every account here is [`ImapAccount::offline`] over one scripted connection, so the tags run
//! on in order (`a3`, `a4`, …) across calls: a call that needed a second connection would fail.

use engine_core::error::FailureClass;
use futures_util::StreamExt;

use super::*;
use crate::{
    ImapAccount,
    mock::{MockStream, Recorded, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK Gimap ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN completed\r\n";
const GMAIL_CAPABILITY: &str = "* CAPABILITY IMAP4rev1 X-GM-EXT-1 CONDSTORE\r\na2 OK done\r\n";

const LIST: &str = concat!(
    r#"* LIST (\HasNoChildren) "/" "INBOX""#,
    "\r\n",
    r#"* LIST (\HasChildren \Noselect) "/" "[Gmail]""#,
    "\r\n",
    r#"* LIST (\All \HasNoChildren) "/" "[Gmail]/All Mail""#,
    "\r\n",
    r#"* LIST (\HasNoChildren \Junk) "/" "[Gmail]/Spam""#,
    "\r\n",
    r#"* LIST (\HasNoChildren \Trash) "/" "[Gmail]/Trash""#,
    "\r\n",
    "a3 OK Success\r\n",
);

fn select(tag: &str, validity: u32) -> String {
    format!(
        "* 3 EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n* OK [UIDNEXT 9] n\r\n\
         * OK [HIGHESTMODSEQ 70] m\r\n{tag} OK [READ-WRITE] Success\r\n"
    )
}

async fn gmail_over(capability: &str, rest: &[&str]) -> (ImapAccount<MockStream>, Recorded) {
    let mut server = vec![GREETING, LOGIN_OK, capability];
    server.extend_from_slice(rest);
    let (stream, recorded) = MockStream::new(script(&server));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice@example.com", "pw").await.unwrap();
    connection.negotiate().await.unwrap();
    (ImapAccount::offline(connection, None), recorded)
}

#[tokio::test]
async fn a_server_without_gmails_extension_offers_no_reader() {
    let (account, _) = gmail_over("* CAPABILITY IMAP4rev1\r\na2 OK done\r\n", &[]).await;
    assert!(account.gmail().is_none());
}

#[tokio::test]
async fn the_folders_are_found_by_role_once_and_every_read_reuses_the_open_folder() {
    let select = select("a4", 11);
    let replies = [
        LIST,
        &select,
        "* SEARCH 2 8\r\na5 OK done\r\n",
        concat!(
            r#"* 1 FETCH (UID 2 FLAGS (\Seen) X-GM-MSGID 20 X-GM-THRID 20 X-GM-LABELS ("\\Inbox") MODSEQ (71))"#,
            "\r\na6 OK done\r\n"
        ),
        "* 2 FETCH (UID 8 X-GM-MSGID 80)\r\na7 OK done\r\n",
    ];
    let (account, recorded) = gmail_over(GMAIL_CAPABILITY, &replies).await;
    let gmail = account.gmail().unwrap();

    let state = gmail.open(GmailFolder::All).await.unwrap().unwrap();
    assert_eq!(
        (
            state.uid_validity,
            state.uid_next,
            state.highest_modseq,
            state.exists
        ),
        (11, 9, 70, 3)
    );
    assert_eq!(
        gmail
            .search(GmailFolder::All, 11, &GmailQuery::default())
            .await
            .unwrap(),
        vec![2, 8]
    );
    let changed = gmail.changed(GmailFolder::All, 11, 9, 70).await.unwrap();
    assert_eq!(
        (changed[0].gmail_id, changed[0].labels.clone()),
        (20, vec!["\\Inbox".to_owned()])
    );
    assert_eq!(
        gmail.ids(GmailFolder::All, 11, &[8]).await.unwrap(),
        vec![(8, 80)]
    );

    let sent = written(&recorded);
    assert_eq!(sent.matches("LIST").count(), 1, "{sent}");
    assert_eq!(sent.matches("SELECT").count(), 1, "{sent}");
    assert!(
        sent.contains(r#"a4 SELECT "[Gmail]/All Mail" (CONDSTORE)"#),
        "{sent}"
    );
}

#[tokio::test]
async fn a_read_planned_against_another_validity_asks_for_a_fresh_sync() {
    let select = select("a4", 12);
    let (account, _) = gmail_over(GMAIL_CAPABILITY, &[LIST, &select]).await;
    let gmail = account.gmail().unwrap();
    let err = gmail
        .messages(GmailFolder::All, 11, &[1])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::NeedsResync);
}

#[tokio::test]
async fn a_hidden_spam_folder_is_absent_and_a_hidden_all_mail_is_refused() {
    let without_spam = concat!(
        r#"* LIST (\All \HasNoChildren) "/" "[Gmail]/All Mail""#,
        "\r\n",
        "a3 OK Success\r\n",
    );
    let (account, _) = gmail_over(GMAIL_CAPABILITY, &[without_spam]).await;
    let gmail = account.gmail().unwrap();
    assert_eq!(gmail.open(GmailFolder::Spam).await.unwrap(), None);
    let err = gmail
        .search(GmailFolder::Trash, 1, &GmailQuery::default())
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);

    let (account, _) = gmail_over(GMAIL_CAPABILITY, &["a3 OK Success\r\n"]).await;
    let err = account
        .gmail()
        .unwrap()
        .open(GmailFolder::All)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn bodies_stream_under_their_gmail_ids_from_wherever_each_is_filed() {
    let first = "From: a@example.com\r\n\r\nfirst\r\n";
    let second = "From: b@example.com\r\n\r\nsecond\r\n";
    let replies = [
        LIST.to_owned(),
        // All Mail holds one of the two.
        "* 3 EXISTS\r\n* OK [UIDVALIDITY 11] v\r\na4 OK [READ-ONLY] done\r\n".to_owned(),
        "* SEARCH 5\r\na5 OK done\r\n".to_owned(),
        "* 1 FETCH (UID 5 X-GM-MSGID 100)\r\na6 OK done\r\n".to_owned(),
        format!(
            "* 1 FETCH (UID 5 BODY[] {{{}}}\r\n{first})\r\na7 OK done\r\n",
            first.len()
        ),
        // Trash holds the other.
        "* 1 EXISTS\r\n* OK [UIDVALIDITY 13] v\r\na8 OK [READ-ONLY] done\r\n".to_owned(),
        "* SEARCH 2\r\na9 OK done\r\n".to_owned(),
        "* 1 FETCH (UID 2 X-GM-MSGID 200)\r\na10 OK done\r\n".to_owned(),
        format!(
            "* 1 FETCH (UID 2 BODY[] {{{}}}\r\n{second})\r\na11 OK done\r\n",
            second.len()
        ),
    ];
    let replies: Vec<&str> = replies.iter().map(String::as_str).collect();
    let (account, recorded) = gmail_over(GMAIL_CAPABILITY, &replies).await;
    let gmail = account.gmail().unwrap();

    let ids = [100, 200];
    let got: Vec<(u64, Vec<u8>)> = gmail
        .sources(&ids)
        .map(|item| item.map(|(id, raw)| (id, raw.as_bytes().to_vec())))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        got,
        vec![
            (100, first.as_bytes().to_vec()),
            (200, second.as_bytes().to_vec())
        ]
    );
    let sent = written(&recorded);
    assert!(sent.contains(r#"a8 EXAMINE "[Gmail]/Trash""#), "{sent}");
    assert!(
        sent.contains("a9 UID SEARCH X-GM-MSGID 200\r\n"),
        "only the id still wanted: {sent}"
    );
}

#[tokio::test]
async fn a_refused_body_read_ends_the_stream_with_its_failure() {
    let replies = [LIST, "a4 NO [NONEXISTENT] Unknown Mailbox\r\n"];
    let (account, _) = gmail_over(GMAIL_CAPABILITY, &replies).await;
    let gmail = account.gmail().unwrap();
    let ids = [100];
    let got: Vec<_> = gmail.sources(&ids).collect().await;
    assert_eq!(got.len(), 1);
    assert!(got[0].is_err());
}
