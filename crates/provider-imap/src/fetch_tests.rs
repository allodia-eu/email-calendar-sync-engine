//! Offline tests for [`fetch_message_source`], driven over a scripted mock stream.

use engine_core::{error::FailureClass, ids::ProviderKey};

use super::fetch_message_source;
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
/// An `EXAMINE` whose `UIDVALIDITY` matches the key below (`v7`). A body read opens
/// the mailbox read-only.
const EXAMINE_V7: &str = "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na2 OK [READ-ONLY] done\r\n";

/// The provider key for INBOX UID 42 under UIDVALIDITY 7.
fn target() -> ProviderKey {
    ProviderKey::new("imap:v7:u42@INBOX").unwrap()
}

/// Builds the untagged `BODY[]` literal response for `body` at `uid`, framed exactly
/// as a server echoes `UID FETCH … (BODY.PEEK[])`.
fn body_response(uid: u32, body: &str) -> String {
    format!(
        "* 3 FETCH (UID {uid} BODY[] {{{}}}\r\n{body})\r\na3 OK FETCH completed\r\n",
        body.len()
    )
}

async fn logged_in(server: Vec<u8>) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

#[tokio::test]
async fn fetch_examines_then_returns_the_raw_body() {
    let body = "From: a@b\r\nSubject: Hi\r\n\r\nHello body — multi\r\nline\r\n";
    let server = script(&[GREETING, LOGIN_OK, EXAMINE_V7, &body_response(42, body)]);
    let (mut conn, recorded) = logged_in(server).await;

    let raw = fetch_message_source(&mut conn, &target()).await.unwrap();
    assert_eq!(raw.as_bytes(), body.as_bytes());

    let sent = written(&recorded);
    // A read-only EXAMINE, not a write-intent SELECT.
    assert!(sent.contains("a2 EXAMINE \"INBOX\""), "{sent}");
    assert!(sent.contains("a3 UID FETCH 42 (BODY.PEEK[])"), "{sent}");
}

#[tokio::test]
async fn a_body_containing_the_literal_framing_round_trips_exactly() {
    // The body itself contains `BODY[] {3}` and a stray `)` — the parser must frame
    // by the first (real) `{n}` length, not by scanning the payload.
    let body = "X-Note: BODY[] {3}\r\n\r\n)not the end\r\n";
    let server = script(&[GREETING, LOGIN_OK, EXAMINE_V7, &body_response(42, body)]);
    let (mut conn, _recorded) = logged_in(server).await;

    let raw = fetch_message_source(&mut conn, &target()).await.unwrap();
    assert_eq!(raw.as_bytes(), body.as_bytes());
}

#[tokio::test]
async fn an_expunged_uid_is_a_conflict() {
    // The mailbox is intact (UIDVALIDITY 7) but UID 42 was expunged since the last
    // sync, so `UID FETCH` is a tagged OK with no FETCH data. That is a Conflict
    // (re-sync, then drop), not a permanent failure.
    let no_data = "a3 OK FETCH completed\r\n";
    let server = script(&[GREETING, LOGIN_OK, EXAMINE_V7, no_data]);
    let (mut conn, _recorded) = logged_in(server).await;

    let err = fetch_message_source(&mut conn, &target())
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_fetch_for_another_uid_does_not_supply_the_body() {
    // An unsolicited FETCH for UID 99 (a piggybacked flag update) carries a BODY[]
    // literal, but it is not our UID 42 — it must be ignored, yielding a Conflict
    // rather than caching the wrong message's bytes.
    let other = body_response(99, "WRONG MESSAGE BODY");
    let server = script(&[GREETING, LOGIN_OK, EXAMINE_V7, &other]);
    let (mut conn, _recorded) = logged_in(server).await;

    let err = fetch_message_source(&mut conn, &target())
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn uidvalidity_mismatch_is_a_conflict() {
    // The mailbox now reports UIDVALIDITY 99 but the key was synthesized under 7:
    // every prior key is stale, so the fetch is a Conflict (re-sync, then retry) —
    // never a read of a renumbered UID space.
    let examine_v99 = "* 3 EXISTS\r\n* OK [UIDVALIDITY 99] v\r\na2 OK [READ-ONLY] done\r\n";
    let server = script(&[GREETING, LOGIN_OK, examine_v99]);
    let (mut conn, recorded) = logged_in(server).await;

    let err = fetch_message_source(&mut conn, &target())
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(
        !written(&recorded).contains("UID FETCH"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn an_unparseable_key_is_invalid_state() {
    // A foreign/garbage key never reaches the wire — it is rejected before EXAMINE.
    let server = script(&[GREETING, LOGIN_OK]);
    let (mut conn, recorded) = logged_in(server).await;

    let key = ProviderKey::new("jmap:Mxyz").unwrap();
    let err = fetch_message_source(&mut conn, &key).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        !written(&recorded).contains("EXAMINE"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_key_mailbox_with_crlf_is_rejected_before_the_wire() {
    // `ProviderKey` only forbids empty, so a crafted key's mailbox could carry CR/LF;
    // since the transport's `quote` escapes only "/\, admitting it would inject a
    // second command. The fetch must be rejected before any IMAP command is sent.
    let server = script(&[GREETING, LOGIN_OK]);
    let (mut conn, recorded) = logged_in(server).await;

    let evil = ProviderKey::new("imap:v7:u42@INBOX\r\na9 DELETE INBOX").unwrap();
    let err = fetch_message_source(&mut conn, &evil).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        !written(&recorded).contains("EXAMINE") && !written(&recorded).contains("DELETE"),
        "{}",
        written(&recorded)
    );
}

/// A `BODY[]` response like [`body_response`], completing under `tag`.
fn tagged_body(tag: &str, uid: u32, body: &str) -> String {
    format!(
        "* 3 FETCH (UID {uid} BODY[] {{{}}}\r\n{body})\r\n{tag} OK FETCH completed\r\n",
        body.len()
    )
}

#[tokio::test]
async fn a_second_read_in_the_open_mailbox_skips_the_examine() {
    // A warm reads a folder's bodies one after another; opening the folder before each one
    // doubles the round trips for an answer the session already holds.
    let server = script(&[
        GREETING,
        LOGIN_OK,
        EXAMINE_V7,
        &tagged_body("a3", 42, "first"),
        &tagged_body("a4", 43, "second"),
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    fetch_message_source(&mut conn, &target()).await.unwrap();
    let second = ProviderKey::new("imap:v7:u43@INBOX").unwrap();
    let raw = fetch_message_source(&mut conn, &second).await.unwrap();

    assert_eq!(raw.as_bytes(), b"second");
    let sent = written(&recorded);
    assert_eq!(sent.matches("EXAMINE").count(), 1, "{sent}");
    assert!(sent.contains("a4 UID FETCH 43 (BODY.PEEK[])"), "{sent}");
}

#[tokio::test]
async fn a_read_in_another_mailbox_opens_that_one() {
    let examine_sent = "* 1 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na4 OK [READ-ONLY] done\r\n";
    let server = script(&[
        GREETING,
        LOGIN_OK,
        EXAMINE_V7,
        &tagged_body("a3", 42, "inbox"),
        examine_sent,
        &tagged_body("a5", 5, "sent"),
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    fetch_message_source(&mut conn, &target()).await.unwrap();
    let in_sent = ProviderKey::new("imap:v7:u5@Sent").unwrap();
    let raw = fetch_message_source(&mut conn, &in_sent).await.unwrap();

    assert_eq!(raw.as_bytes(), b"sent");
    assert!(written(&recorded).contains("a4 EXAMINE \"Sent\""));
}

#[tokio::test]
async fn a_refused_examine_leaves_no_mailbox_to_reuse() {
    // A failed `SELECT`/`EXAMINE` deselects whatever was open (RFC 9051 §6.3.1). A session
    // that still believed INBOX was open would read the next INBOX UID without opening it,
    // and the server would answer for no mailbox at all.
    let server = script(&[
        GREETING,
        LOGIN_OK,
        EXAMINE_V7,
        &tagged_body("a3", 42, "first"),
        "a4 NO [NONEXISTENT] no such mailbox\r\n",
        "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na5 OK [READ-ONLY] done\r\n",
        &tagged_body("a6", 43, "again"),
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    fetch_message_source(&mut conn, &target()).await.unwrap();
    let gone = ProviderKey::new("imap:v7:u1@Gone").unwrap();
    assert!(fetch_message_source(&mut conn, &gone).await.is_err());
    let again = ProviderKey::new("imap:v7:u43@INBOX").unwrap();
    fetch_message_source(&mut conn, &again).await.unwrap();

    assert!(written(&recorded).contains("a5 EXAMINE \"INBOX\""));
}

#[tokio::test(start_paused = true)]
async fn a_body_that_stops_arriving_fails_as_a_lost_connection() {
    use tokio::io::AsyncWriteExt;

    // The server starts the literal and then sends nothing more, as a socket does whose path
    // died mid-response. Without a bound the read waits for the OS to give up retransmitting.
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    server
        .write_all(
            concat!(
                "* OK ready\r\n",
                "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na1 OK [READ-ONLY] done\r\n",
                "* 3 FETCH (UID 42 BODY[] {100}\r\nFrom: a@b\r\n",
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut conn = Connection::open(client).await.unwrap();

    let err = fetch_message_source(&mut conn, &target())
        .await
        .unwrap_err();

    // Retryable: the connection is gone, not the message, so a fresh one may well succeed.
    assert_eq!(err.class(), FailureClass::Retryable, "{err}");
    assert!(err.to_string().contains("sent nothing"), "{err}");
    drop(server);
}
