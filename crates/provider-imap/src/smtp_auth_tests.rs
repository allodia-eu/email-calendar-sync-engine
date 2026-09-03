//! Offline tests for SMTP authentication, driven through the whole [`crate::smtp`]
//! conversation over a mock stream.
//!
//! Driving `send` rather than the `AUTH` step alone is deliberate: it pins where the
//! authentication sits in the sequence (after `EHLO`, before `MAIL FROM`), which is the
//! part a real server rejects. What a mock still cannot check is whether the server
//! accepts the bytes — that is `tests/live_imap_oauth.rs`.

use engine_core::{error::FailureClass, ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use engine_rfc5322::assemble_message;
use time::macros::datetime;

use super::*;
use crate::{
    error::ImapError,
    mock::{MockStream, script, written},
    smtp::{Disposition, send},
};

/// RFC 7628 §4.4's SMTP error challenge, verbatim.
const CHALLENGE: &str = "334 eyJzdGF0dXMiOiJpbnZhbGlkX3Rva2VuIiwic2NoZW1lcyI6ImJlYXJlciIsInNjb3BlIjoiaHR0cHM6Ly9tYWlsLmV4YW1wbGUuY29tLyJ9\r\n";

/// Runs a submission that authenticates with `credentials` over `server_script`,
/// returning the outcome and the bytes the client wrote.
async fn submit(
    server_script: Vec<u8>,
    credentials: &Credentials,
) -> (ImapResult<crate::smtp::SmtpResult>, String) {
    let draft = Draft::new(
        MessageIdHeader::new("smtp-oauth@host").unwrap(),
        EmailAddress::new("alice@example.com"),
        vec![EmailAddress::new("bob@example.com")],
        "Subject line",
        "hi",
    );
    let message = assemble_message(&draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap();
    let (stream, recorded) = MockStream::new(server_script);
    let outcome = send(
        stream,
        "example.com",
        "alice@example.com",
        &["bob@example.com".to_owned()],
        &message,
        Some(SmtpAuth {
            credentials,
            host: "smtp.example.com",
            port: Some(465),
        }),
    )
    .await;
    (outcome, written(&recorded))
}

/// The tail of a successful submission, after the `235`.
const DELIVERY: &str =
    "250 2.1.0 OK\r\n250 2.1.5 OK\r\n354 go ahead\r\n250 2.0.0 queued\r\n221 bye\r\n";

#[tokio::test]
async fn a_token_authenticates_with_the_mechanism_the_server_advertised() {
    let credentials = Credentials::oauth2("alice@example.com", "ya29.token");
    let (outcome, sent) = submit(
        script(&[
            "220 mail ESMTP\r\n",
            "250-mail\r\n250-STARTTLS\r\n250 AUTH PLAIN LOGIN XOAUTH2 OAUTHBEARER\r\n",
            "235 2.7.0 Accepted\r\n",
            DELIVERY,
        ]),
        &credentials,
    )
    .await;
    assert_eq!(
        outcome.expect("delivered").disposition,
        Disposition::Delivered
    );

    // The preferred mechanism, with the credential inline (RFC 4954's initial response).
    let expected = crate::sasl::Mechanism::OAuthBearer
        .initial_response(
            "alice@example.com",
            "ya29.token",
            "smtp.example.com",
            Some(465),
        )
        .expect("clean credential");
    assert!(
        sent.contains(&format!("AUTH OAUTHBEARER {expected}\r\n")),
        "{sent}"
    );
    // The token is never in the clear, and never as a password.
    assert!(!sent.contains("ya29.token"), "token leaked: {sent}");
    assert!(!sent.contains("AUTH PLAIN"), "{sent}");
    // Authentication precedes the envelope, or a strict server rejects `MAIL FROM`.
    let auth_at = sent.find("AUTH OAUTHBEARER").expect("AUTH issued");
    let mail_at = sent.find("MAIL FROM").expect("MAIL issued");
    assert!(auth_at < mail_at, "AUTH must precede MAIL: {sent}");
}

#[tokio::test]
async fn a_microsoft_style_server_gets_the_vendor_mechanism() {
    // The fallback half: Exchange Online documents only `XOAUTH2` — and has switched
    // basic auth off — so a server without the preferred mechanism must not be left
    // unauthenticated.
    let credentials = Credentials::oauth2("alice@example.com", "tok");
    let (outcome, sent) = submit(
        script(&[
            "220 mail ESMTP\r\n",
            "250-mail\r\n250 AUTH PLAIN LOGIN XOAUTH2\r\n",
            "235 2.7.0 Accepted\r\n",
            DELIVERY,
        ]),
        &credentials,
    )
    .await;
    outcome.expect("delivered");
    assert!(sent.contains("AUTH XOAUTH2 "), "{sent}");
}

#[tokio::test]
async fn a_rejected_token_is_acknowledged_and_reported_with_the_servers_reason() {
    let credentials = Credentials::oauth2("alice@example.com", "expired");
    let (outcome, sent) = submit(
        script(&[
            "220 mail ESMTP\r\n",
            "250-mail\r\n250 AUTH OAUTHBEARER\r\n",
            CHALLENGE,
            "535-5.7.1 Username and Password not accepted\r\n535 5.7.1 see the docs\r\n",
        ]),
        &credentials,
    )
    .await;

    let err = outcome.expect_err("a rejected token must fail");
    assert_eq!(err.failure_class(), FailureClass::Authentication);
    let detail = err.to_string();
    assert!(detail.contains("invalid_token"), "{detail}");
    assert!(detail.contains("535"), "{detail}");

    // The `334` is the rejection, not a request for more credential: acknowledging it is
    // what makes the server send the `535` at all (RFC 7628 §3.2.3).
    assert!(sent.contains("\r\nAQ==\r\n"), "{sent}");
    // Nothing was submitted afterwards.
    assert!(!sent.contains("MAIL FROM"), "{sent}");
}

#[tokio::test]
async fn a_rejected_xoauth2_token_is_acknowledged_with_an_empty_line() {
    let credentials = Credentials::oauth2("alice@example.com", "expired");
    let (outcome, sent) = submit(
        script(&[
            "220 mail ESMTP\r\n",
            "250-mail\r\n250 AUTH XOAUTH2\r\n",
            CHALLENGE,
            "535 5.7.1 Username and Password not accepted\r\n",
        ]),
        &credentials,
    )
    .await;
    assert!(matches!(outcome, Err(ImapError::Auth(_))), "{outcome:?}");
    assert!(sent.ends_with("\r\n\r\n"), "{sent:?}");
}

#[tokio::test]
async fn a_flat_refusal_with_no_challenge_is_still_an_authentication_error() {
    // Not every server describes the rejection: some answer the `AUTH` line with a bare
    // `535`. There is nothing to acknowledge then, and nothing to wait for.
    let credentials = Credentials::oauth2("alice@example.com", "expired");
    let (outcome, sent) = submit(
        script(&[
            "220 mail ESMTP\r\n",
            "250-mail\r\n250 AUTH XOAUTH2\r\n",
            "535 5.7.1 Username and Password not accepted\r\n",
        ]),
        &credentials,
    )
    .await;
    let err = outcome.expect_err("a rejected token must fail");
    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert!(err.to_string().contains("535"), "{err}");
    assert!(!sent.contains("MAIL FROM"), "{sent}");
}

#[tokio::test]
async fn a_server_offering_no_oauth_mechanism_says_what_it_does_offer() {
    let credentials = Credentials::oauth2("alice@example.com", "tok");
    let (outcome, sent) = submit(
        script(&["220 mail ESMTP\r\n", "250-mail\r\n250 AUTH PLAIN LOGIN\r\n"]),
        &credentials,
    )
    .await;

    let err = outcome.expect_err("nothing to present");
    assert_eq!(err.failure_class(), FailureClass::Authentication);
    let detail = err.to_string();
    assert!(
        detail.contains("PLAIN") && detail.contains("LOGIN"),
        "{detail}"
    );
    // A token is never downgraded into a password attempt.
    assert!(!sent.contains("AUTH "), "{sent}");
    assert!(!sent.contains("tok"), "token must not be sent: {sent}");
}

#[tokio::test]
async fn a_challenge_that_says_nothing_still_produces_the_servers_status_code() {
    // Some servers send the `334` with no payload. There is then nothing to decode, and
    // the error must not read as though a reason were attached.
    let credentials = Credentials::oauth2("alice@example.com", "expired");
    let (outcome, sent) = submit(
        script(&[
            "220 mail ESMTP\r\n",
            "250-mail\r\n250 AUTH OAUTHBEARER\r\n",
            "334 \r\n",
            "535 5.7.1 Username and Password not accepted\r\n",
        ]),
        &credentials,
    )
    .await;
    let err = outcome.expect_err("a rejected token must fail");
    let detail = err.to_string();
    assert!(detail.contains("535"), "{detail}");
    assert!(
        !detail.contains("()"),
        "an absent reason must not render as empty: {detail}"
    );
    assert!(sent.contains("\r\nAQ==\r\n"), "{sent}");
}

#[tokio::test]
async fn an_smtp_server_advertising_no_mechanisms_at_all_still_explains_itself() {
    let credentials = Credentials::oauth2("alice@example.com", "tok");
    let (outcome, _sent) = submit(
        script(&["220 mail ESMTP\r\n", "250-mail\r\n250 PIPELINING\r\n"]),
        &credentials,
    )
    .await;
    let err = outcome.expect_err("nothing to present");
    assert!(err.to_string().contains("none"), "{err}");
}

#[test]
fn mechanisms_are_read_per_line_and_in_both_spellings() {
    let line = |text: &str| vec![text.to_owned()];
    assert_eq!(
        advertised_mechanisms(&line("AUTH PLAIN LOGIN XOAUTH2")),
        ["PLAIN", "LOGIN", "XOAUTH2"]
    );
    // The legacy spelling glues the first mechanism to the keyword.
    assert_eq!(
        advertised_mechanisms(&line("AUTH=PLAIN LOGIN")),
        ["PLAIN", "LOGIN"]
    );
    // The keyword only counts at the start of its own line: a greeting that happens to
    // name a mechanism in prose is not an offer of it. Reading a *joined* reply is what
    // would get this wrong, which is why `ehlo` keeps the lines apart.
    let reply = vec![
        "mail.example.com says it will not do XOAUTH2".to_owned(),
        "SIZE 35882577".to_owned(),
        "AUTH OAUTHBEARER".to_owned(),
    ];
    assert_eq!(advertised_mechanisms(&reply), ["OAUTHBEARER"]);
    // Nothing advertised is an empty list, not a panic.
    assert!(advertised_mechanisms(&line("PIPELINING")).is_empty());
    assert!(advertised_mechanisms(&line("")).is_empty());
    assert!(advertised_mechanisms(&line("AUTH")).is_empty());
}
