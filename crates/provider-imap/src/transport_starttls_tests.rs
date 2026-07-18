//! Offline tests for the IMAP `STARTTLS` preamble, over a scripted mock stream.
//!
//! The mock serves canned bytes regardless of the request, so these assert the
//! *command shape* the client emits (`CAPABILITY` then `STARTTLS`) and the two safety
//! properties that do not depend on a real server: refusing to proceed when `STARTTLS`
//! is not advertised (no cleartext-credential downgrade), and rejecting data buffered
//! past the `STARTTLS` response (the command-injection guard). The full upgrade against
//! a real TLS server is the gated live test.

use crate::{
    error::ImapError,
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1 STARTTLS] Stalwart ready\r\n";

#[tokio::test]
async fn start_tls_issues_capability_then_starttls_when_advertised() {
    let server = script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 STARTTLS AUTH=PLAIN\r\na1 OK CAPABILITY done\r\n",
        "a2 OK Begin TLS negotiation now\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);

    let mut conn = Connection::open(stream).await.unwrap();
    conn.start_tls().await.expect("STARTTLS negotiated");

    let sent = written(&recorded);
    // Exactly `CAPABILITY` then `STARTTLS`, correctly tagged and in order.
    let cap = sent.find("a1 CAPABILITY").expect("CAPABILITY issued");
    let tls = sent.find("a2 STARTTLS").expect("STARTTLS issued");
    assert!(cap < tls, "CAPABILITY must precede STARTTLS: {sent}");

    // The script ends exactly at the STARTTLS OK, so nothing is buffered — the socket
    // is clean to hand to the TLS layer.
    assert!(conn.into_inner_stream().is_ok());
}

#[tokio::test]
async fn start_tls_refuses_when_starttls_not_advertised() {
    // Neither the greeting nor the CAPABILITY reply advertises STARTTLS.
    let server = script(&[
        "* OK [CAPABILITY IMAP4rev1] ready\r\n",
        "* CAPABILITY IMAP4rev1 AUTH=PLAIN\r\na1 OK CAPABILITY done\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);

    let mut conn = Connection::open(stream).await.unwrap();
    let err = conn.start_tls().await.expect_err("must refuse");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");

    // The client asked CAPABILITY but never sent STARTTLS (nor any credentials) — it
    // aborts before the login it would otherwise send in the clear.
    let sent = written(&recorded);
    assert!(sent.contains("a1 CAPABILITY"), "{sent}");
    assert!(!sent.contains("STARTTLS"), "must not send STARTTLS: {sent}");
}

#[tokio::test]
async fn resume_wraps_the_upgraded_stream_without_a_greeting() {
    // Post-upgrade the server sends no fresh greeting — the client logs in directly.
    // The script has NO `* OK` line: a `Connection::resume` that tried to read one
    // would hang or misparse; instead it drives straight into LOGIN.
    let server = script(&["a1 OK LOGIN completed\r\n"]);
    let (stream, recorded) = MockStream::new(server);

    let mut conn = Connection::resume(stream);
    conn.login("alice@test.local", "pw")
        .await
        .expect("login on the resumed (greeting-less) connection");

    // Tags restart at 1 on the resumed connection.
    assert!(
        written(&recorded).starts_with("a1 LOGIN"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn into_inner_stream_rejects_data_buffered_past_starttls() {
    // A conformant server sends nothing between the STARTTLS OK and the client's TLS
    // ClientHello. Extra plaintext here simulates a STARTTLS-stripping injection: it
    // must not be carried across the TLS boundary.
    let server = script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 STARTTLS\r\na1 OK CAPABILITY done\r\n",
        "a2 OK Begin TLS negotiation now\r\n",
        "a3 OK injected pre-TLS command\r\n",
    ]);
    let (stream, _recorded) = MockStream::new(server);

    let mut conn = Connection::open(stream).await.unwrap();
    conn.start_tls().await.expect("STARTTLS negotiated");

    let err = conn
        .into_inner_stream()
        .expect_err("buffered data must be rejected");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
}
