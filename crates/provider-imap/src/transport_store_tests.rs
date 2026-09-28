//! `NAMESPACE` and pipelined `MYRIGHTS` over a scripted stream: what goes on the wire, and
//! that a refusal in the middle of a pipeline costs only its own answer.

use super::RIGHTS_BATCH;
use crate::{
    mock::{MockStream, Recorded, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

async fn logged_in(server: &[&str]) -> (Connection<MockStream>, Recorded) {
    let mut parts = vec![GREETING, LOGIN_OK];
    parts.extend_from_slice(server);
    let (stream, recorded) = MockStream::new(script(&parts));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice", "pw").await.unwrap();
    (connection, recorded)
}

#[tokio::test]
async fn namespace_reads_the_servers_answer() {
    let (mut connection, recorded) = logged_in(&[
        "* NAMESPACE ((\"\" \"/\")) ((\"Shared Folders\" \"/\")) NIL\r\n",
        "a2 OK NAMESPACE completed\r\n",
    ])
    .await;
    let namespaces = connection.namespace().await.unwrap();
    assert!(written(&recorded).ends_with("a2 NAMESPACE\r\n"));
    assert_eq!(namespaces.other_users[0].prefix, "Shared Folders");
}

#[tokio::test]
async fn namespace_decodes_a_prefix_only_on_a_rev1_session() {
    let answer = [
        "* NAMESPACE ((\"\" \"/\")) NIL ((\"&ANY-ffentlich/\" \"/\"))\r\n",
        "a2 OK done\r\n",
    ];
    let (mut rev1, _) = logged_in(&answer).await;
    assert_eq!(
        rev1.namespace().await.unwrap().shared[0].prefix,
        "Öffentlich"
    );

    let (mut rev2, _) = logged_in(&answer).await;
    rev2.force_enabled("IMAP4rev2");
    assert_eq!(
        rev2.namespace().await.unwrap().shared[0].prefix,
        "&ANY-ffentlich"
    );
}

#[tokio::test]
async fn myrights_pipelines_and_a_refusal_costs_only_its_own_answer() {
    let (mut connection, recorded) = logged_in(&[
        "* MYRIGHTS \"INBOX\" rliteswkxpa\r\na2 OK done\r\n",
        // A folder the server will not answer for: its refusal must be read to its tag,
        // or the next answer would be taken for this one.
        "a3 NO Mailbox does not exist.\r\n",
        "* MYRIGHTS \"Shared Folders/bob@test.local/INBOX\" rl\r\na4 OK done\r\n",
        // An answer with no MYRIGHTS line at all.
        "a5 OK done\r\n",
    ])
    .await;
    let rights = connection
        .myrights(&[
            "INBOX".to_owned(),
            "Shared Folders".to_owned(),
            "Shared Folders/bob@test.local/INBOX".to_owned(),
            "Überweisungen".to_owned(),
        ])
        .await
        .unwrap();
    assert_eq!(
        rights,
        [
            Some("rliteswkxpa".to_owned()),
            None,
            Some("rl".to_owned()),
            None
        ]
    );
    // All four went out together, each name quoted and — this being a rev1 session —
    // encoded the way the server lists it.
    assert!(written(&recorded).ends_with(
        "a2 MYRIGHTS \"INBOX\"\r\n\
         a3 MYRIGHTS \"Shared Folders\"\r\n\
         a4 MYRIGHTS \"Shared Folders/bob@test.local/INBOX\"\r\n\
         a5 MYRIGHTS \"&ANw-berweisungen\"\r\n"
    ));
}

#[tokio::test]
async fn answers_that_complete_out_of_order_still_land_on_their_own_folders() {
    // What Stalwart does with a pipelined batch (RFC 9051 §5.5 allows it): completions in
    // an order of the server's choosing, each answer's `* MYRIGHTS` line nowhere near its
    // tag. Pairing by position would give the INBOX bob's `rl`.
    let (mut connection, _) = logged_in(&[
        "* MYRIGHTS \"Shared Folders/bob@test.local/INBOX\" rl\r\n",
        "a4 OK MYRIGHTS completed\r\n",
        "* MYRIGHTS \"INBOX\" rliteswkxpa\r\n",
        "a3 NO Mailbox does not exist.\r\n",
        "* 3 EXISTS\r\n",
        "a2 OK MYRIGHTS completed\r\n",
    ])
    .await;
    let rights = connection
        .myrights(&[
            "INBOX".to_owned(),
            "Shared Folders".to_owned(),
            "Shared Folders/bob@test.local/INBOX".to_owned(),
        ])
        .await
        .unwrap();
    assert_eq!(
        rights,
        [Some("rliteswkxpa".to_owned()), None, Some("rl".to_owned())]
    );
}

#[tokio::test]
async fn a_rev2_session_pairs_an_answer_by_the_name_as_written() {
    // UTF-8 on the wire, so an `&` is a character rather than the start of modified UTF-7;
    // decoding it as rev1 would lose the answer.
    let (mut connection, _) = logged_in(&["* MYRIGHTS \"Q&A\" lr\r\na2 OK done\r\n"]).await;
    connection.force_enabled("IMAP4rev2");
    let rights = connection.myrights(&["Q&A".to_owned()]).await.unwrap();
    assert_eq!(rights, [Some("lr".to_owned())]);
}

#[tokio::test]
async fn a_completion_for_a_tag_never_sent_is_a_protocol_error() {
    let (mut connection, _) = logged_in(&["a9 OK stray\r\n"]).await;
    assert!(connection.myrights(&["INBOX".to_owned()]).await.is_err());
}

#[tokio::test]
async fn myrights_batches_a_long_folder_list() {
    // One batch more than fits: every answer still lands in its folder's slot.
    let count = RIGHTS_BATCH + 1;
    let answers: Vec<String> = (0..count)
        .map(|i| format!("* MYRIGHTS \"f{i}\" lr\r\na{} OK done\r\n", i + 2))
        .collect();
    let answers: Vec<&str> = answers.iter().map(String::as_str).collect();
    let (mut connection, recorded) = logged_in(&answers).await;
    let folders: Vec<String> = (0..count).map(|i| format!("f{i}")).collect();
    let rights = connection.myrights(&folders).await.unwrap();
    assert_eq!(rights.len(), count);
    assert!(rights.iter().all(|r| r.as_deref() == Some("lr")));
    assert!(written(&recorded).contains(&format!("a{} MYRIGHTS \"f{}\"", count + 1, count - 1)));
}

#[tokio::test]
async fn a_broken_connection_is_an_error_not_an_unknown() {
    // The script ends before the first answer: that is a dead session, not "no rights".
    let (mut connection, _) = logged_in(&[]).await;
    assert!(connection.myrights(&["INBOX".to_owned()]).await.is_err());
}
