//! Offline tests for the capability/`ENABLE` handshake: which dialect a session ends up
//! speaking, and which extensions it may then use.
//!
//! Split from `transport_tests.rs` (the line protocol itself) because this is one
//! responsibility — what the client and server agree on before any mailbox is touched —
//! and it is the part where a wrong answer is silent rather than loud.

use engine_core::error::FailureClass;

use super::*;
use crate::mock::{MockStream, script, written};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] Stalwart ready\r\n";

#[tokio::test]
async fn one_enable_turns_on_the_dialect_and_every_extension_that_needs_it() {
    // A dual-revision server advertising QRESYNC: both go in the same ENABLE, so the
    // session pays one round trip for the dialect and the extension together.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 IMAP4rev1 ENABLE CONDSTORE QRESYNC UIDPLUS\r\na2 OK CAPABILITY done\r\n",
        "* ENABLED IMAP4rev2 QRESYNC\r\na3 OK ENABLE successful\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    assert!(!conn.qresync_enabled());

    conn.negotiate().await.unwrap();

    assert!(conn.qresync_enabled());
    // IDLE was never advertised, but rev2 folds it in (RFC 9051 Appendix E) — the point
    // of enabling the dialect at all.
    assert!(conn.idle_available());
    // …and rev2 mailbox names are UTF-8, so nothing is decoded as modified UTF-7.
    assert!(!conn.names_are_modified_utf7());

    let sent = written(&recorded);
    assert!(sent.contains("a2 CAPABILITY"), "{sent}");
    assert!(sent.contains("a3 ENABLE IMAP4rev2 QRESYNC"), "{sent}");
    assert_eq!(sent.matches("ENABLE ").count(), 1, "one round trip: {sent}");
}

#[tokio::test]
async fn a_rev1_only_server_enables_only_what_it_advertised() {
    // No rev2 on offer, so the session stays rev1: names are modified UTF-7, and an
    // extension is available only because this server named it.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev1 IDLE QRESYNC UIDPLUS\r\na2 OK CAPABILITY done\r\n",
        "* ENABLED QRESYNC\r\na3 OK ENABLE successful\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate().await.unwrap();

    assert!(conn.qresync_enabled());
    assert!(conn.idle_available());
    assert!(conn.names_are_modified_utf7());
    let sent = written(&recorded);
    assert!(sent.contains("a3 ENABLE QRESYNC"), "{sent}");
    assert!(!sent.contains("IMAP4rev2"), "{sent}");
}

#[tokio::test]
async fn advertising_rev2_without_confirming_it_leaves_the_session_on_rev1() {
    // The trap: the server offers rev2 and answers OK, but enables nothing (RFC 5161
    // §3.1). Treating the advertisement as the answer would leave the client reading
    // modified UTF-7 mailbox names as though they were UTF-8.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 IMAP4rev1\r\na2 OK CAPABILITY done\r\n",
        "* ENABLED\r\na3 OK ENABLE successful\r\n",
    ]);
    let (stream, _recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate().await.unwrap();

    assert!(conn.names_are_modified_utf7(), "rev2 was never confirmed");
    assert!(
        !conn.idle_available(),
        "nor is anything it would have folded in"
    );
}

#[tokio::test]
async fn nothing_to_enable_costs_no_round_trip() {
    // A plain rev1 server with no enable-requiring extension: the ENABLE is skipped
    // entirely rather than sent empty.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev1 IDLE UIDPLUS\r\na2 OK CAPABILITY done\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate().await.unwrap();

    assert!(!conn.qresync_enabled());
    assert!(conn.idle_available());
    assert!(!written(&recorded).contains("ENABLE"), "no ENABLE sent");
}

#[tokio::test]
async fn an_ok_that_enabled_nothing_leaves_the_baseline() {
    // The server advertises QRESYNC and answers ENABLE with a tagged OK, but the
    // `* ENABLED` line is empty (it enabled nothing, RFC 5161) — so we must stay on the
    // baseline rather than issue CONDSTORE/VANISHED commands the server would reject.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 QRESYNC\r\na2 OK CAPABILITY done\r\n",
        "* ENABLED\r\na3 OK ENABLE successful\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate().await.unwrap();
    assert!(
        !conn.qresync_enabled(),
        "a bare ENABLED list does not enable QRESYNC"
    );
}

#[tokio::test]
async fn negotiation_tolerates_an_enable_rejection() {
    // A server that advertises QRESYNC but rejects ENABLE leaves the session on the
    // baseline rather than failing the whole connection.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 QRESYNC\r\na2 OK CAPABILITY done\r\n",
        "a3 NO ENABLE not right now\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate().await.unwrap();
    assert!(
        !conn.qresync_enabled(),
        "an ENABLE NO falls back to baseline"
    );
}

#[tokio::test]
async fn negotiation_propagates_a_transport_error() {
    // A non-NO/BAD failure during ENABLE (here a protocol violation) is propagated,
    // not swallowed — only an advertised-but-refused NO/BAD falls back to baseline.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 QRESYNC\r\na2 OK CAPABILITY done\r\n",
        "+ unexpected continuation\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let err = conn.negotiate().await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(!conn.qresync_enabled());
}
