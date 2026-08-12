//! Offline tests for the unread-count paths: the `STATUS` parser, and the
//! per-mailbox probing fallback driven over a mock stream.

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
    sync::SyncUpdate,
};
use engine_provider::Provider;

use super::{parse_status_unseen, unseen_by_probing};
use crate::{
    ImapProvider,
    mock::{MockStream, script, written},
    parse::ListRow,
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

fn lines(text: &str) -> Vec<Vec<u8>> {
    // The transport hands the parsers each untagged response body — the bytes after
    // the leading `* ` — so the fixtures are written the same way.
    text.lines()
        .map(|line| line.trim().as_bytes().to_vec())
        .collect()
}

fn row(name: &str, attributes: &[&str]) -> ListRow {
    ListRow {
        attributes: attributes.iter().map(|a| (*a).to_owned()).collect(),
        delimiter: Some("/".to_owned()),
        name: name.to_owned(),
    }
}

async fn connection(server: Vec<u8>) -> Connection<MockStream> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn
}

#[test]
fn reads_the_unseen_count_wherever_it_sits_in_the_attribute_list() {
    let counts = parse_status_unseen(&lines(
        r#"STATUS "INBOX" (MESSAGES 1200 UNSEEN 545 UIDNEXT 1201)
           STATUS "Trash" (UNSEEN 4)
           STATUS "Sent" (MESSAGES 90 UIDNEXT 91)"#,
    ));
    assert_eq!(counts.get("INBOX"), Some(&545));
    assert_eq!(counts.get("Trash"), Some(&4));
    // The server returned no UNSEEN for Sent — absent, not zero.
    assert_eq!(counts.get("Sent"), None);
}

#[test]
fn zero_unseen_is_a_real_answer() {
    let counts = parse_status_unseen(&lines(r#"STATUS "Archive" (UNSEEN 0)"#));
    assert_eq!(counts.get("Archive"), Some(&0));
}

#[test]
fn non_status_lines_and_malformed_ones_are_skipped_not_fatal() {
    let counts = parse_status_unseen(&lines(
        r#"LIST (\HasNoChildren) "/" "INBOX"
           STATUS "Good" (UNSEEN 7)
           STATUS "NoAttributes"
           STATUS "NotANumber" (UNSEEN twelve)
           a4 OK LIST done"#,
    ));
    assert_eq!(counts.len(), 1);
    assert_eq!(counts.get("Good"), Some(&7));
}

#[tokio::test]
async fn list_status_asks_once_and_pairs_every_count_to_its_mailbox() {
    let response = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                    * STATUS \"INBOX\" (UNSEEN 545)\r\n\
                    * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
                    * STATUS \"Sent\" (UNSEEN 0)\r\n\
                    a2 OK LIST done\r\n";
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, response]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let (rows, counts) = conn.list_with_unseen().await.unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(counts.get("INBOX"), Some(&545));
    assert_eq!(counts.get("Sent"), Some(&0));
    let sent = written(&recorded);
    assert!(
        sent.contains(r#"a2 LIST "" "*" RETURN (STATUS (UNSEEN))"#),
        "{sent}"
    );
    // One round trip for both questions is the whole point of the extension.
    assert_eq!(sent.matches("STATUS").count(), 1, "{sent}");
}

/// A `LIST-STATUS` answer in the shape a Dovecot server gives it: unquoted mailbox names
/// where no quoting is needed, `SPECIAL-USE` attributes only because they were asked for,
/// and a completion detail that is prose ending in a period.
const DOVECOT_SHAPED_LIST: &str = "* LIST (\\UnMarked \\Sent) \"/\" Sent\r\n\
                                   * STATUS Sent (UNSEEN 0)\r\n\
                                   * LIST (\\UnMarked \\Junk) \"/\" Spam\r\n\
                                   * STATUS Spam (UNSEEN 1)\r\n\
                                   * LIST () \"/\" INBOX\r\n\
                                   * STATUS INBOX (UNSEEN 14)\r\n\
                                   a2 OK List completed (0.003 + 0.000 + 0.002 secs).\r\n";

#[tokio::test]
async fn the_folder_list_asks_for_roles_and_counts_together() {
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, DOVECOT_SHAPED_LIST]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.negotiated = crate::capability::Negotiated::from_capabilities(&["SPECIAL-USE".to_owned()]);

    let (rows, counts) = conn.list_with_unseen().await.unwrap();

    // An extended `LIST` returns only the extended data its options name, so both are
    // asked for in the one round trip the extension exists to buy.
    let sent = written(&recorded);
    assert!(
        sent.contains(r#"a2 LIST "" "*" RETURN (SPECIAL-USE STATUS (UNSEEN))"#),
        "{sent}"
    );
    assert_eq!(sent.matches("STATUS").count(), 1, "{sent}");

    let role_of = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .and_then(|row| crate::mail::mailbox_from_list(row, true))
            .and_then(|mailbox| mailbox.role)
    };
    assert_eq!(role_of("Sent"), Some(MailboxRole::Sent));
    assert_eq!(role_of("Spam"), Some(MailboxRole::Junk));
    assert_eq!(role_of("INBOX"), Some(MailboxRole::Inbox));
    assert_eq!(counts.get("Spam"), Some(&1));
}

#[tokio::test]
async fn the_completion_line_is_not_a_folder() {
    let mut conn = connection(script(&[GREETING, LOGIN_OK, DOVECOT_SHAPED_LIST])).await;

    let (rows, _counts) = conn.list_with_unseen().await.unwrap();

    // `List completed (…).` parses as four items whose first word is the keyword and
    // whose last is a bare `.`; read as a row it puts a folder named "." in the sidebar,
    // gives it a sync scope, and syncs it.
    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, ["Sent", "Spam", "INBOX"], "{names:?}");
}

#[tokio::test]
async fn a_server_without_special_use_is_not_asked_for_it() {
    let response = "* LIST () \"/\" INBOX\r\n\
                    * STATUS INBOX (UNSEEN 14)\r\n\
                    a2 OK List completed (0.001 secs).\r\n";
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, response]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.list_with_unseen().await.unwrap();

    // A return option the server never advertised is a `BAD`, which would cost the
    // folder list entirely rather than only its roles.
    let sent = written(&recorded);
    assert!(
        sent.contains(r#"a2 LIST "" "*" RETURN (STATUS (UNSEEN))"#),
        "{sent}"
    );
    assert!(!sent.contains("SPECIAL-USE"), "{sent}");
}

#[tokio::test]
async fn probing_asks_per_mailbox_and_skips_noselect_containers() {
    let responses = "* STATUS \"INBOX\" (UNSEEN 545)\r\n\
                     a2 OK STATUS done\r\n\
                     * STATUS \"Work/Clients\" (UNSEEN 2)\r\n\
                     a3 OK STATUS done\r\n";
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, responses]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let counts = unseen_by_probing(
        &mut conn,
        &[
            row("INBOX", &["\\HasNoChildren"]),
            // A hierarchy node: STATUS on one is an error, so it is never asked.
            row("Work", &["\\Noselect", "\\HasChildren"]),
            row("Work/Clients", &["\\HasNoChildren"]),
        ],
    )
    .await
    .unwrap();

    assert_eq!(counts.get("INBOX"), Some(&545));
    assert_eq!(counts.get("Work/Clients"), Some(&2));
    assert_eq!(counts.get("Work"), None);
    let sent = written(&recorded);
    assert!(sent.contains(r#"a2 STATUS "INBOX" (UNSEEN)"#), "{sent}");
    assert!(
        sent.contains(r#"a3 STATUS "Work/Clients" (UNSEEN)"#),
        "{sent}"
    );
    assert!(!sent.contains(r#"STATUS "Work" "#), "{sent}");
}

#[tokio::test]
async fn a_refused_mailbox_costs_only_its_own_count() {
    let responses = "a2 NO STATUS not permitted\r\n\
                     * STATUS \"Sent\" (UNSEEN 3)\r\n\
                     a3 OK STATUS done\r\n";
    let mut conn = connection(script(&[GREETING, LOGIN_OK, responses])).await;

    let counts = unseen_by_probing(
        &mut conn,
        &[
            row("Restricted", &["\\HasNoChildren"]),
            row("Sent", &["\\HasNoChildren", "\\Sent"]),
        ],
    )
    .await
    .unwrap();

    // The refusal is skipped; the folder after it is still counted.
    assert_eq!(counts.get("Restricted"), None);
    assert_eq!(counts.get("Sent"), Some(&3));
}

/// The mailboxes one `sync_mailboxes` pass produced, by name and unread count.
async fn synced_counts(server: Vec<u8>, list_status: bool) -> Vec<(String, Option<u32>)> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    if list_status {
        conn.negotiated =
            crate::capability::Negotiated::from_capabilities(&["LIST-STATUS".to_owned()]);
    }
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let sync = provider
        .sync_mailboxes(&AccountId::try_from("acct-1").unwrap(), None)
        .await
        .unwrap();
    match sync.update {
        SyncUpdate::Snapshot { objects, .. } => objects
            .into_iter()
            .map(|mailbox| (mailbox.name, mailbox.unread_count))
            .collect(),
        SyncUpdate::Delta { .. } => panic!("the folder list is always a snapshot"),
    }
}

#[tokio::test]
async fn a_folder_list_carries_each_mailbox_its_own_count() {
    let response = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                    * STATUS \"INBOX\" (UNSEEN 545)\r\n\
                    * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
                    * STATUS \"Sent\" (UNSEEN 0)\r\n\
                    * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                    a2 OK LIST done\r\n";
    let counts = synced_counts(script(&[GREETING, LOGIN_OK, response]), true).await;

    assert!(counts.contains(&("INBOX".to_owned(), Some(545))));
    assert!(counts.contains(&("Sent".to_owned(), Some(0))));
    // Archive got a LIST row but no STATUS line: uncounted, which is not zero.
    assert!(counts.contains(&("Archive".to_owned(), None)));
}

#[tokio::test]
async fn without_the_extension_the_same_counts_arrive_one_probe_at_a_time() {
    let list = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
                a2 OK LIST done\r\n";
    let probes = "* STATUS \"INBOX\" (UNSEEN 545)\r\n\
                  a3 OK STATUS done\r\n\
                  * STATUS \"Sent\" (UNSEEN 0)\r\n\
                  a4 OK STATUS done\r\n";
    let counts = synced_counts(script(&[GREETING, LOGIN_OK, list, probes]), false).await;

    // The user gets the same badge either way; only the round trips differ.
    assert!(counts.contains(&("INBOX".to_owned(), Some(545))));
    assert!(counts.contains(&("Sent".to_owned(), Some(0))));
}
