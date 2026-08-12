//! Offline tests for role-folder resolution, the already-placed probe, and key derivation,
//! driven over mock streams.

use engine_core::ids::MessageIdHeader;

use super::{Filing, find_placed_copy, placed_key, resolve_filing_folder};
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN completed\r\n";

async fn connection(parts: &[&str]) -> (Connection<MockStream>, crate::mock::Recorded) {
    let mut all = vec![GREETING, LOGIN_OK];
    all.extend_from_slice(parts);
    let (stream, recorded) = MockStream::new(script(&all));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

fn message_id() -> MessageIdHeader {
    MessageIdHeader::new("placed-probe@test.local").unwrap()
}

#[tokio::test]
async fn the_role_folder_wins_over_a_conventionally_named_one() {
    // The server's real Sent folder is named in Dutch and tagged `\Sent`; a plain folder
    // called "Sent" also exists. Resolving by role must pick the tagged one.
    let (mut conn, _) = connection(&["* LIST (\\HasNoChildren) \"/\" \"Sent\"\r\n\
         * LIST (\\HasNoChildren \\Sent) \"/\" \"Verzonden items\"\r\n\
         a2 OK LIST done\r\n"])
    .await;

    let folder = resolve_filing_folder(&mut conn, Filing::Sent)
        .await
        .unwrap();
    assert_eq!(folder, "Verzonden items");
}

/// A role folder whose name is modified UTF-7 resolves to the **wire** name. Decoding it
/// here would hand `APPEND` a name the server never advertised, and would put the decoded
/// form inside every message key built from it.
#[tokio::test]
async fn a_role_folder_resolves_to_its_wire_name_not_its_display_name() {
    let (mut conn, recorded) = connection(&[
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"&ZeVnLIqe-\"\r\na2 OK LIST done\r\n",
        "+ OK literal\r\n",
        "a3 OK [APPENDUID 7 3] APPEND completed\r\n",
    ])
    .await;

    let (folder, append_uid) = super::append_to_role_folder(&mut conn, Filing::Sent, b"raw")
        .await
        .unwrap();

    assert_eq!(folder, "&ZeVnLIqe-");
    assert_eq!(append_uid, Some((7, 3)));
    assert!(
        written(&recorded).contains("APPEND \"&ZeVnLIqe-\""),
        "the APPEND addresses the wire name: {}",
        written(&recorded)
    );
    // And the key the caller derives embeds that same wire name, so the message stays
    // addressable after a restart.
    assert_eq!(
        placed_key(
            &folder,
            Filing::Sent.key_prefix(),
            append_uid,
            &message_id()
        )
        .as_str(),
        "imap:v7:u3@&ZeVnLIqe-"
    );
}

#[tokio::test]
async fn no_advertised_role_folder_falls_back_to_the_conventional_name() {
    let (mut conn, recorded) = connection(&[
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\na2 OK LIST done\r\n",
        "a3 OK CREATE done\r\n",
    ])
    .await;

    let folder = resolve_filing_folder(&mut conn, Filing::Sent)
        .await
        .unwrap();

    assert_eq!(folder, "Sent");
    assert!(written(&recorded).contains("CREATE \"Sent\""));
}

/// The probe that makes retrying a placement safe: it finds the copy a first, apparently
/// failed attempt actually committed, so the retry does not append a second one.
#[tokio::test]
async fn the_probe_finds_an_already_placed_copy() {
    let (mut conn, recorded) = connection(&[
        "* 8 EXISTS\r\n* OK [UIDVALIDITY 4242] valid\r\na2 OK [READ-WRITE] SELECT done\r\n",
        "* SEARCH 17\r\na3 OK SEARCH completed\r\n",
    ])
    .await;

    let found = find_placed_copy(&mut conn, "Sent", &message_id())
        .await
        .unwrap();

    assert_eq!(found, Some((4242, 17)));
    assert!(
        written(&recorded).contains("UID SEARCH HEADER Message-ID \"placed-probe@test.local\""),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn the_probe_reports_nothing_when_the_copy_is_absent() {
    let (mut conn, _) = connection(&[
        "* 8 EXISTS\r\n* OK [UIDVALIDITY 4242] valid\r\na2 OK [READ-WRITE] SELECT done\r\n",
        "* SEARCH\r\na3 OK SEARCH completed\r\n",
    ])
    .await;

    assert_eq!(
        find_placed_copy(&mut conn, "Sent", &message_id())
            .await
            .unwrap(),
        None
    );
}

/// Without UIDPLUS there is no `APPENDUID`, so the key is derived from the `Message-ID`
/// and the next sync of that folder resolves it to the real one.
#[test]
fn a_key_without_appenduid_falls_back_to_the_message_id() {
    assert_eq!(
        placed_key("Sent", Filing::Sent.key_prefix(), None, &message_id()).as_str(),
        "sent:placed-probe@test.local"
    );
    assert_eq!(
        placed_key("Drafts", Filing::Drafts.key_prefix(), None, &message_id()).as_str(),
        "draft:placed-probe@test.local"
    );
}
