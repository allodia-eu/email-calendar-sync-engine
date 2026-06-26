//! Offline tests for [`fetch_message_source`], driven over a scripted mock stream.

use super::fetch_message_source;
use crate::mock::{MockStream, script, written};
use crate::transport::Connection;
use engine_core::error::FailureClass;
use engine_core::ids::ProviderKey;

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
