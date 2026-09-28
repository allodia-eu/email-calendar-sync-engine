//! Offline tests for the extended folder `LIST` and the per-folder data riding on it.

use engine_core::mail::MailboxRole;

use crate::{
    capability::Negotiated,
    mock::{MockStream, Recorded, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

/// A logged-in rev1 session that advertised `extensions`, about to receive `response`.
async fn session(extensions: &[&str], response: &str) -> (Connection<MockStream>, Recorded) {
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, response]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let advertised: Vec<String> = extensions.iter().map(|e| (*e).to_owned()).collect();
    conn.negotiated = Negotiated::from_capabilities(&advertised);
    (conn, recorded)
}

#[tokio::test]
async fn a_session_without_the_extensions_asks_a_plain_list_and_gets_no_extras() {
    let response = "* LIST () \"/\" INBOX\r\na2 OK List completed.\r\n";
    let (mut conn, recorded) = session(&["ACL"], response).await;

    let listing = conn.list_folders("*").await.unwrap();

    assert_eq!(listing.rows.len(), 1);
    // Not asked is not "none": the caller must ask some other way.
    assert!(listing.unseen.is_none() && listing.rights.is_none());
    assert!(written(&recorded).ends_with("a2 LIST \"\" \"*\"\r\n"));
}

#[tokio::test]
async fn list_myrights_pairs_every_rights_line_to_its_mailbox() {
    // Dovecot's order: each row's `LIST`, `STATUS`, then `MYRIGHTS`, never the rights
    // first (RFC 8440 §3). A row the server has no rights for simply has no line.
    let response = "* LIST () \"/\" INBOX\r\n\
                    * STATUS INBOX (UNSEEN 2)\r\n\
                    * MYRIGHTS INBOX lrwstipekxacd\r\n\
                    * LIST () \"/\" \"shared/bob@test.local\"\r\n\
                    * STATUS \"shared/bob@test.local\" (UNSEEN 13)\r\n\
                    * MYRIGHTS \"shared/bob@test.local\" lr\r\n\
                    * LIST (\\Sent) \"/\" Sent\r\n\
                    a2 OK List completed.\r\n";
    let (mut conn, recorded) = session(&["LIST-STATUS", "LIST-MYRIGHTS"], response).await;

    let listing = conn.list_folders("*").await.unwrap();

    let rights = listing.rights.unwrap();
    assert_eq!(rights.len(), 2);
    assert_eq!(rights["INBOX"], "lrwstipekxacd");
    assert_eq!(rights["shared/bob@test.local"], "lr");
    assert_eq!(listing.unseen.unwrap()["shared/bob@test.local"], 13);
    let sent = written(&recorded);
    assert!(
        sent.ends_with("a2 LIST \"\" \"*\" RETURN (STATUS (UNSEEN) MYRIGHTS)\r\n"),
        "{sent}"
    );
}

#[tokio::test]
async fn list_status_asks_once_and_pairs_every_count_to_its_mailbox() {
    let response = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                    * STATUS \"INBOX\" (UNSEEN 545)\r\n\
                    * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
                    * STATUS \"Sent\" (UNSEEN 0)\r\n\
                    a2 OK LIST done\r\n";
    let (mut conn, recorded) = session(&["LIST-STATUS"], response).await;

    let listing = conn.list_folders("*").await.unwrap();
    let (rows, counts) = (listing.rows, listing.unseen.unwrap());

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
    let (mut conn, recorded) = session(&["SPECIAL-USE", "LIST-STATUS"], DOVECOT_SHAPED_LIST).await;

    let listing = conn.list_folders("*").await.unwrap();
    let (rows, counts) = (listing.rows, listing.unseen.unwrap());

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
    let (mut conn, _) = session(&["LIST-STATUS"], DOVECOT_SHAPED_LIST).await;

    let rows = conn.list_folders("*").await.unwrap().rows;

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
    let (mut conn, recorded) = session(&["LIST-STATUS"], response).await;

    conn.list_folders("*").await.unwrap();

    // A return option the server never advertised is a `BAD`, which would cost the
    // folder list entirely rather than only its roles.
    let sent = written(&recorded);
    assert!(
        sent.contains(r#"a2 LIST "" "*" RETURN (STATUS (UNSEEN))"#),
        "{sent}"
    );
    assert!(!sent.contains("SPECIAL-USE"), "{sent}");
}
