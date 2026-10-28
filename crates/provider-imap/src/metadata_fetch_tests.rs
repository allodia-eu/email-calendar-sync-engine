//! Offline tests for the two-command metadata fetch: which rows the header section
//! settles, and that `BODYSTRUCTURE` is asked for only the rest, on the page path and on
//! the streamed backfill alike.

use core::fmt::Write as _;

use engine_core::{ids::MailboxId, sync::SyncWindow};
use futures_util::StreamExt;

use super::*;
use crate::{
    capability::Negotiated,
    mock::{MockStream, script, written},
    parse::parse_fetch,
    stream::stream_email,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
const TEXT: &str = "Content-Type: text/html; charset=utf-8\r\n\r\n";
const MIXED: &str = "Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n";
const ALTERNATIVE: &str = "Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n";
const WITH_PDF: &str = r#"(("TEXT" "PLAIN" NIL NIL NIL "7BIT" 2 1)("APPLICATION" "PDF" ("NAME" "a.pdf") NIL NIL "BASE64" 12 NIL ("ATTACHMENT" ("FILENAME" "a.pdf")) NIL) "MIXED")"#;
const TEXT_ONLY: &str = r#"(("TEXT" "PLAIN" NIL NIL NIL "7BIT" 2 1)("TEXT" "HTML" NIL NIL NIL "7BIT" 2 1) "ALTERNATIVE")"#;

/// One metadata row as a server answers [`FETCH_ITEMS`], the header section as a literal.
fn metadata_row(uid: u32, headers: &str) -> String {
    format!(
        "* {uid} FETCH (UID {uid} FLAGS () ENVELOPE (NIL \"s\" NIL NIL NIL NIL NIL NIL NIL \"<m{uid}@h>\") \
         BODY[HEADER.FIELDS (REFERENCES CONTENT-TYPE CONTENT-DISPOSITION)] {{{}}}\r\n{headers})\r\n",
        headers.len()
    )
}

fn response(tag: &str, rows: &[String]) -> String {
    let mut out = rows.concat();
    write!(out, "{tag} OK FETCH done\r\n").unwrap();
    out
}

async fn logged_in(server: &[&str]) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(script(server));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

#[test]
fn the_header_section_settles_a_text_body_and_leaves_a_multipart_open() {
    let line = |uid, headers| {
        metadata_row(uid, headers)
            .trim_start_matches("* ")
            .trim_end()
            .as_bytes()
            .to_vec()
    };
    let rows = parse_fetch(&[line(1, TEXT), line(2, MIXED), line(3, "\r\n")]).unwrap();
    assert_eq!(rows[0].has_attachment, Some(false));
    assert_eq!(rows[1].has_attachment, None);
    // No `Content-Type` at all is `text/plain`.
    assert_eq!(rows[2].has_attachment, Some(false));
    assert!(!needs_structure(&rows[0]));
    assert!(needs_structure(&rows[1]));
}

#[tokio::test]
async fn bodystructure_is_asked_for_only_the_rows_the_headers_left_open() {
    let metadata = response(
        "a2",
        &[
            metadata_row(1, TEXT),
            metadata_row(2, MIXED),
            metadata_row(3, TEXT),
            metadata_row(4, ALTERNATIVE),
        ],
    );
    // The structure answer carries an unsolicited flag update for a row it did not ask
    // about, which settles nothing.
    let structure = format!(
        "* 2 FETCH (UID 2 BODYSTRUCTURE {WITH_PDF})\r\n* 3 FETCH (UID 3 FLAGS (\\Seen))\r\n\
         * 4 FETCH (UID 4 BODYSTRUCTURE {TEXT_ONLY})\r\na3 OK FETCH done\r\n"
    );
    let (mut conn, recorded) = logged_in(&[GREETING, LOGIN_OK, &metadata, &structure]).await;

    let rows = conn.uid_fetch_metadata("1:4").await.unwrap();

    let flags: Vec<_> = rows
        .iter()
        .map(|row| (row.uid, row.has_attachment))
        .collect();
    assert_eq!(
        flags,
        [
            (1, Some(false)),
            (2, Some(true)),
            (3, Some(false)),
            (4, Some(false))
        ]
    );
    let sent = written(&recorded);
    assert!(
        sent.contains(&format!("a2 UID FETCH 1:4 ({FETCH_ITEMS})")),
        "{sent}"
    );
    assert!(
        sent.contains("a3 UID FETCH 2,4 (UID BODYSTRUCTURE)"),
        "{sent}"
    );
}

#[tokio::test]
async fn rows_the_headers_all_settle_cost_one_command() {
    let metadata = response("a2", &[metadata_row(1, TEXT), metadata_row(2, "\r\n")]);
    let (mut conn, recorded) = logged_in(&[GREETING, LOGIN_OK, &metadata]).await;

    let rows = conn.uid_fetch_metadata("1:2").await.unwrap();

    assert!(rows.iter().all(|row| row.has_attachment == Some(false)));
    assert!(!written(&recorded).contains("BODYSTRUCTURE"));
}

#[tokio::test]
async fn no_structure_command_names_more_messages_than_the_server_allows() {
    let metadata = response(
        "a2",
        &[
            metadata_row(1, MIXED),
            metadata_row(2, TEXT),
            metadata_row(3, MIXED),
            metadata_row(4, ALTERNATIVE),
        ],
    );
    let first = format!(
        "* 1 FETCH (UID 1 BODYSTRUCTURE {WITH_PDF})\r\n* 3 FETCH (UID 3 BODYSTRUCTURE {TEXT_ONLY})\r\n\
         a3 OK FETCH done\r\n"
    );
    let second = format!("* 4 FETCH (UID 4 BODYSTRUCTURE {WITH_PDF})\r\na4 OK FETCH done\r\n");
    let (mut conn, recorded) = logged_in(&[GREETING, LOGIN_OK, &metadata, &first, &second]).await;
    conn.negotiated = Negotiated::from_capabilities(&["MESSAGELIMIT=2".to_owned()]);

    let rows = conn.uid_fetch_metadata("1:4").await.unwrap();

    let flags: Vec<_> = rows.iter().map(|row| row.has_attachment).collect();
    assert_eq!(flags, [Some(true), Some(false), Some(false), Some(true)]);
    let sent = written(&recorded);
    assert!(
        sent.contains("a3 UID FETCH 1,3 (UID BODYSTRUCTURE)"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID FETCH 4 (UID BODYSTRUCTURE)"),
        "{sent}"
    );
}

/// A row goes out as its headers arrive, an open one as having no attachment, which is how an
/// open row reads; the settle then sends again only the rows whose structure shows one. Holding
/// the open rows for the settle kept a first sync's list empty for seconds per group on Yahoo,
/// where most messages are multipart and the structure fetch is slow.
#[tokio::test]
async fn the_streamed_backfill_sends_rows_before_settling_and_corrects_only_what_changed() {
    let select = "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\n* OK [UIDNEXT 4] n\r\n\
                  a2 OK [READ-WRITE] done\r\n";
    let metadata = response(
        "a3",
        &[
            metadata_row(1, MIXED),
            metadata_row(2, TEXT),
            metadata_row(3, MIXED),
        ],
    );
    // UID 3 was expunged between the two commands, so only UID 1 comes back.
    let structure = format!("* 1 FETCH (UID 1 BODYSTRUCTURE {WITH_PDF})\r\na4 OK FETCH done\r\n");
    let (mut conn, recorded) =
        logged_in(&[GREETING, LOGIN_OK, select, &metadata, &structure]).await;
    let mailbox = MailboxId::try_from("INBOX").unwrap();

    let mut stream = Box::pin(stream_email(
        &mut conn,
        None,
        &mailbox,
        None,
        SyncWindow::full(),
        0,
        1,
    ));
    let mut sent_rows = Vec::new();
    let mut present = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        present.extend(chunk.present.iter().map(|key| key.as_str().to_owned()));
        for message in chunk.changed {
            sent_rows.push((message.id.key().as_str().to_owned(), message.has_attachment));
        }
    }
    drop(stream);

    assert_eq!(
        sent_rows,
        [
            ("imap:v7:u1@INBOX".to_owned(), false),
            ("imap:v7:u2@INBOX".to_owned(), false),
            ("imap:v7:u3@INBOX".to_owned(), false),
            ("imap:v7:u1@INBOX".to_owned(), true),
        ]
    );
    present.sort();
    assert_eq!(
        present,
        ["imap:v7:u1@INBOX", "imap:v7:u2@INBOX", "imap:v7:u3@INBOX"],
        "a corrected row is counted present once"
    );
    let sent = written(&recorded);
    assert!(
        sent.contains("a4 UID FETCH 1,3 (UID BODYSTRUCTURE)"),
        "{sent}"
    );
}

/// With the group in one chunk, a correction finds its first copy still waiting and replaces it:
/// the chunk names each message once.
#[tokio::test]
async fn a_correction_replaces_a_copy_that_has_not_gone_out() {
    let select = "* 2 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\n* OK [UIDNEXT 3] n\r\n\
                  a2 OK [READ-WRITE] done\r\n";
    let metadata = response("a3", &[metadata_row(1, MIXED), metadata_row(2, TEXT)]);
    let structure = format!("* 1 FETCH (UID 1 BODYSTRUCTURE {WITH_PDF})\r\na4 OK FETCH done\r\n");
    let (mut conn, _) = logged_in(&[GREETING, LOGIN_OK, select, &metadata, &structure]).await;
    let mailbox = MailboxId::try_from("INBOX").unwrap();

    let mut stream = Box::pin(stream_email(
        &mut conn,
        None,
        &mailbox,
        None,
        SyncWindow::full(),
        0,
        0,
    ));
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.unwrap());
    }
    drop(stream);

    let rows: Vec<_> = chunks
        .iter()
        .flat_map(|chunk| &chunk.changed)
        .map(|message| (message.id.key().as_str().to_owned(), message.has_attachment))
        .collect();
    assert_eq!(
        rows,
        [
            ("imap:v7:u1@INBOX".to_owned(), true),
            ("imap:v7:u2@INBOX".to_owned(), false),
        ]
    );
}
