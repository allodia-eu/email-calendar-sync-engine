//! Offline tests for SMTP `AUTH PLAIN`, over a mock stream: the exchange itself, and how a
//! refusal is classified, since a refusal read as a bad password makes a host ask the user
//! to sign in again.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use engine_rfc5322::assemble_message;
use time::{OffsetDateTime, macros::datetime};

use super::*;
use crate::mock::{MockStream, script, written};

fn draft(to: &[&str], body: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new("smtp-auth@host").unwrap(),
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
async fn send_authenticates_with_auth_plain_over_the_stream() {
    let server = script(&[
        "220 mail ESMTP\r\n",
        "250-mail\r\n250 AUTH PLAIN\r\n",
        "235 2.7.0 authenticated\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("alice@test.local", "s3cret")),
    )
    .await
    .unwrap();

    assert_eq!(result.disposition, Disposition::Delivered);
    let sent = written(&recorded);
    assert!(sent.contains("AUTH PLAIN "), "{sent}");
    // The password is base64 in the SASL token, never in the clear.
    assert!(
        !sent.contains("s3cret"),
        "credentials leaked in the clear: {sent}"
    );
}

#[tokio::test]
async fn an_auth_rejection_is_an_authentication_error() {
    let server = script(&[
        "220 mail\r\n",
        "250 AUTH PLAIN\r\n",
        "535 5.7.8 bad credentials\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));

    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("alice@test.local", "wrong")),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Authentication
    );
}

#[tokio::test]
async fn an_auth_deferral_is_rate_limited_not_a_bad_password() {
    // A 4xx is "try again later" (RFC 5321 §4.2.1); RFC 4954 §6 names 454 for a temporary
    // authentication failure. Read as `Authentication`, a host asks the user to sign in
    // again, which cannot help.
    for deferral in [
        "454 4.7.0 Temporary authentication failure\r\n",
        "421 4.7.0 Try again later, closing connection\r\n",
    ] {
        let err = auth_refused_with(deferral).await;
        assert!(
            matches!(err, ImapError::RateLimited(_)),
            "{deferral}: {err:?}"
        );
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::RateLimited,
            "{deferral}"
        );
    }
}

#[tokio::test]
async fn an_auth_asking_for_a_password_transition_stays_an_authentication_error() {
    // The one 4xx RFC 4954 §6 gives AUTH that no wait resolves: the user has to act.
    let err = auth_refused_with("432 4.7.12 A password transition is needed\r\n").await;
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Authentication
    );
}

async fn auth_refused_with(reply: &str) -> ImapError {
    let server = script(&["220 mail\r\n", "250 AUTH PLAIN\r\n", reply]);
    let (stream, _) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));
    send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("alice@test.local", "pw")),
    )
    .await
    .unwrap_err()
}

#[tokio::test]
async fn auth_without_esmtp_is_a_protocol_error() {
    // EHLO is refused (HELO-only), so AUTH cannot run.
    let server = script(&["220 mail\r\n", "502 no EHLO\r\n", "250 OK\r\n"]);
    let (stream, _) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));
    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("user", "pass")),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}
