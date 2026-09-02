//! Offline tests for SMTP `STARTTLS` submission (RFC 3207), over a mock stream.
//!
//! The mock serves canned bytes regardless of the request, so these assert the
//! *command shape*: [`negotiate_starttls`] does `EHLO → STARTTLS` and refuses without
//! the advertised extension (no cleartext auth) or on data buffered past the `220`
//! (the injection guard); [`send_after_starttls`] skips the greeting and runs
//! `EHLO → AUTH → MAIL → RCPT → DATA` over the (notionally) upgraded stream. The full
//! upgrade against a real TLS server is the gated live test.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use engine_rfc5322::assemble_message;
use time::{OffsetDateTime, macros::datetime};

use super::*;
use crate::mock::{MockStream, script, written};

fn draft(to: &[&str], body: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new("smtp-starttls@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        to.iter().map(|t| EmailAddress::new(*t)).collect(),
        "Subject line",
        body,
    )
}

fn fixed_date() -> OffsetDateTime {
    datetime!(2026-06-20 12:00:00 UTC)
}

fn assembled(draft: &Draft) -> Vec<u8> {
    assemble_message(draft, fixed_date()).unwrap()
}

fn recipients(to: &[&str]) -> Vec<String> {
    to.iter().map(|t| (*t).to_owned()).collect()
}

#[tokio::test]
async fn negotiate_starttls_upgrades_when_advertised() {
    let server = script(&[
        "220 mail ESMTP\r\n",
        "250-mail\r\n250-STARTTLS\r\n250 SMTPUTF8\r\n",
        "220 2.0.0 Ready to start TLS\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);

    // Returns the (clean) stream ready for the caller to TLS-wrap.
    negotiate_starttls(stream, "client.test")
        .await
        .expect("STARTTLS negotiated");

    let sent = written(&recorded);
    let ehlo = sent.find("EHLO client.test").expect("EHLO issued");
    let tls = sent.find("STARTTLS").expect("STARTTLS issued");
    assert!(ehlo < tls, "EHLO must precede STARTTLS: {sent}");
}

#[tokio::test]
async fn negotiate_starttls_refuses_without_the_extension() {
    // EHLO reply advertises AUTH but not STARTTLS — authenticating here would be in
    // the clear, so the negotiation aborts before sending STARTTLS.
    let server = script(&["220 mail ESMTP\r\n", "250-mail\r\n250 AUTH PLAIN\r\n"]);
    let (stream, recorded) = MockStream::new(server);

    let err = negotiate_starttls(stream, "client.test")
        .await
        .expect_err("must refuse");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");

    let sent = written(&recorded);
    assert!(sent.contains("EHLO client.test"), "{sent}");
    assert!(!sent.contains("STARTTLS"), "must not send STARTTLS: {sent}");
}

#[tokio::test]
async fn negotiate_starttls_rejects_data_buffered_past_the_220() {
    // A conformant server sends nothing between the STARTTLS 220 and the client's TLS
    // ClientHello; the trailing line simulates an injection that must not survive.
    let server = script(&[
        "220 mail ESMTP\r\n",
        "250-mail\r\n250-STARTTLS\r\n250 OK\r\n",
        "220 2.0.0 Ready to start TLS\r\n",
        "250 injected pre-TLS line\r\n",
    ]);
    let (stream, _recorded) = MockStream::new(server);

    let err = negotiate_starttls(stream, "client.test")
        .await
        .expect_err("buffered data must be rejected");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn negotiate_starttls_reports_a_refused_upgrade() {
    // The server advertises STARTTLS but then refuses the command with a non-220 — the
    // upgrade cannot proceed, so no credentials are ever sent in the clear.
    let server = script(&[
        "220 mail ESMTP\r\n",
        "250-mail\r\n250 STARTTLS\r\n",
        "454 4.7.0 TLS not available right now\r\n",
    ]);
    let (stream, _recorded) = MockStream::new(server);

    let err = negotiate_starttls(stream, "client.test")
        .await
        .expect_err("a refused STARTTLS must error");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn negotiate_starttls_fails_when_neither_ehlo_nor_helo_is_accepted() {
    // A server that greets but rejects both EHLO and HELO: the preamble cannot even
    // learn the extensions, so it aborts before STARTTLS.
    let server = script(&[
        "220 mail ready\r\n",
        "502 5.5.1 EHLO not supported\r\n",
        "502 5.5.1 HELO not supported\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);

    let err = negotiate_starttls(stream, "client.test")
        .await
        .expect_err("EHLO+HELO refusal must error");
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
    let sent = written(&recorded);
    assert!(
        sent.contains("EHLO client.test") && sent.contains("HELO client.test"),
        "{sent}"
    );
    assert!(!sent.contains("STARTTLS"), "must not send STARTTLS: {sent}");
}

#[tokio::test]
async fn send_after_starttls_skips_greeting_and_authenticates() {
    // No 220 greeting in the script: after a STARTTLS upgrade the server sends none,
    // and the client's first line is EHLO.
    let server = script(&[
        "250-mail\r\n250 AUTH PLAIN\r\n",
        "235 2.7.0 Authentication successful\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));

    let result = send_after_starttls(
        stream,
        "client.test",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(SmtpAuth {
            credentials: &crate::credentials::Credentials::password("alice@test.local", "pw"),
            host: "smtp.test.local",
            port: Some(587),
        }),
    )
    .await
    .unwrap();

    assert_eq!(result.disposition, Disposition::Delivered);
    let sent = written(&recorded);
    // The first thing on the wire is EHLO (no greeting read), then AUTH before MAIL.
    assert!(sent.starts_with("EHLO client.test"), "{sent}");
    let auth = sent.find("AUTH PLAIN").expect("AUTH issued");
    let mail = sent.find("MAIL FROM").expect("MAIL issued");
    assert!(auth < mail, "AUTH must precede MAIL: {sent}");
}
