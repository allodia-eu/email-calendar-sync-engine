//! Offline tests for the submission side of the auth probe
//! ([`extensions`]/[`extensions_after_starttls`]), over a mock stream.
//!
//! The gated live test proves the answer survives a real dial; only these can assert
//! what the probe *sends*, which is the half a canned-bytes mock cannot validate for us:
//! `EHLO` and then `QUIT`, and **no** `MAIL`, `RCPT`, `DATA` or `AUTH` — a probe that
//! leaked any of those would be an envelope or a credential on the wire at account
//! setup, before the user has agreed to anything.

use super::*;
use crate::{
    mock::{MockStream, script, written},
    smtp_auth::advertised_mechanisms,
};

/// A submission greeting plus the `EHLO` reply that follows it.
const GREETING: &str = "220 mail.test.local ESMTP\r\n";
const EHLO_REPLY: &str = "250-mail.test.local\r\n250-STARTTLS\r\n250-AUTH PLAIN OAUTHBEARER\r\n\
                          250 SMTPUTF8\r\n";

/// Nothing beyond `EHLO`/`QUIT` may appear on the wire.
fn assert_probe_only(sent: &str) {
    for forbidden in ["MAIL FROM", "RCPT TO", "DATA", "AUTH", "STARTTLS"] {
        assert!(
            !sent.contains(forbidden),
            "probe must not send {forbidden}: {sent}"
        );
    }
}

#[tokio::test]
async fn the_probe_reads_the_ehlo_reply_and_quits() {
    let (stream, recorded) = MockStream::new(script(&[GREETING, EHLO_REPLY, "221 bye\r\n"]));

    let offered = extensions(stream, "client.test")
        .await
        .expect("EHLO accepted");

    assert_eq!(advertised_mechanisms(&offered), ["PLAIN", "OAUTHBEARER"]);
    let sent = written(&recorded);
    let ehlo = sent.find("EHLO client.test").expect("EHLO issued");
    let quit = sent.find("QUIT").expect("QUIT issued");
    assert!(ehlo < quit, "EHLO must precede QUIT: {sent}");
    assert_probe_only(&sent[..quit]);
}

#[tokio::test]
async fn the_post_starttls_probe_sends_no_second_greeting_read() {
    // The upgrade consumed the one greeting, so this entry must go straight to `EHLO`.
    // Scripting *only* the EHLO reply is the assertion: a stray greeting read would
    // consume it and leave the reply unparsed.
    let (stream, recorded) = MockStream::new(script(&[EHLO_REPLY, "221 bye\r\n"]));

    let offered = extensions_after_starttls(stream, "client.test")
        .await
        .expect("EHLO accepted");

    assert_eq!(advertised_mechanisms(&offered), ["PLAIN", "OAUTHBEARER"]);
    let sent = written(&recorded);
    assert!(sent.starts_with("EHLO client.test"), "{sent}");
    assert_probe_only(&sent[..sent.find("QUIT").expect("QUIT issued")]);
}

#[tokio::test]
async fn a_refused_greeting_stops_the_probe_before_ehlo() {
    let (stream, recorded) = MockStream::new(script(&["554 no service here\r\n"]));

    let err = extensions(stream, "client.test")
        .await
        .expect_err("a non-220 greeting is not a session");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
    assert!(
        written(&recorded).is_empty(),
        "nothing may be sent to a server that refused the connection"
    );
}

#[tokio::test]
async fn a_refused_ehlo_and_helo_is_an_error_not_an_empty_offer() {
    // Both refused: reporting `Ok(vec![])` here would read as "this server offers no
    // authentication", which is a different answer from "the question went unanswered".
    let (stream, recorded) = MockStream::new(script(&[
        GREETING,
        "502 command not implemented\r\n",
        "502 command not implemented\r\n",
    ]));

    let err = extensions(stream, "client.test")
        .await
        .expect_err("neither EHLO nor HELO accepted");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
    let sent = written(&recorded);
    assert!(sent.contains("EHLO client.test"), "{sent}");
    assert!(
        sent.contains("HELO client.test"),
        "fell back to HELO: {sent}"
    );
}

#[tokio::test]
async fn a_domain_carrying_a_newline_never_reaches_the_wire() {
    // A host supplies the EHLO domain, and the probe is the first thing an account
    // setup screen runs. One CR would append a second command to the `EHLO` line.
    let (stream, recorded) = MockStream::new(script(&[GREETING]));

    let err = extensions(stream, "client.test\r\nMAIL FROM:<attacker@example.com>")
        .await
        .expect_err("a control character in the domain is refused");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
    assert!(
        !written(&recorded).contains("MAIL FROM"),
        "the injected command must never be written: {}",
        written(&recorded)
    );
}
