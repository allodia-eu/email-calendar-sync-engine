//! Signing in over scripted servers: which mechanism is chosen, the bytes it sends, and what each
//! answer becomes.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;

use super::*;
use crate::mock::{MockStream, Recorded, written};

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("../../tests/fixtures/managesieve/", $name, ".txt")).as_slice()
    };
}

const HOST: &str = "sieve.example.com";

/// A session greeted with `greeting`, whose server answers `answers` after it.
async fn greeted(greeting: &[u8], answers: &[u8]) -> (Session<MockStream>, Recorded) {
    let mut script = greeting.to_vec();
    script.extend_from_slice(answers);
    let (stream, recorded) = MockStream::new(script);
    (Session::open(stream).await.unwrap(), recorded)
}

fn password() -> Credential {
    Credential::password("alice@example.com", "hunter2")
}

#[tokio::test]
async fn a_password_signs_in_over_plain_with_the_initial_response_inline() {
    for (greeting, answer) in [
        (
            fixture!("stalwart_greeting_tls"),
            fixture!("stalwart_authenticate"),
        ),
        (
            fixture!("dovecot_greeting_tls"),
            fixture!("dovecot_authenticate"),
        ),
    ] {
        let (mut session, recorded) = greeted(greeting, answer).await;
        session
            .authenticate(&password(), HOST, Some(4190))
            .await
            .unwrap();
        // base64("\0alice@example.com\0hunter2")
        assert_eq!(
            written(&recorded),
            "AUTHENTICATE \"PLAIN\" \"AGFsaWNlQGV4YW1wbGUuY29tAGh1bnRlcjI=\"\r\n"
        );
    }
}

#[tokio::test]
async fn a_refused_password_is_a_refusal_that_may_ask_for_another() {
    for (greeting, answer) in [
        (
            fixture!("stalwart_greeting_tls"),
            fixture!("stalwart_authenticate_refused"),
        ),
        (
            fixture!("dovecot_greeting_tls"),
            fixture!("dovecot_authenticate_refused"),
        ),
    ] {
        let (mut session, _) = greeted(greeting, answer).await;
        let err = session
            .authenticate(&password(), HOST, None)
            .await
            .unwrap_err();
        assert!(err.is_sign_in_refusal(), "{err:?}");
        assert!(
            !err.to_string().contains("Authentication failed"),
            "no server text: {err}"
        );
        assert_eq!(
            ProviderError::from(err).class(),
            FailureClass::Authentication
        );
    }
}

#[tokio::test]
async fn a_credential_with_no_mechanism_on_offer_sends_nothing() {
    // Before TLS no mechanism is offered at all.
    let (mut session, recorded) = greeted(fixture!("stalwart_greeting"), b"").await;
    let err = session
        .authenticate(&password(), HOST, None)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SieveError::Imap(ImapError::AuthUnsupported(_))
    ));
    assert!(!err.is_sign_in_refusal());
    // Dovecot takes passwords alone: a token has no mechanism there.
    let (mut session, recorded_token) = greeted(fixture!("dovecot_greeting_tls"), b"").await;
    let token = Credential::bearer_for("alice@example.com", "ya29.secret");
    let err = session.authenticate(&token, HOST, None).await.unwrap_err();
    assert!(err.to_string().contains("it offers: PLAIN"), "{err}");
    assert_eq!(written(&recorded), "");
    assert_eq!(written(&recorded_token), "");
}

#[tokio::test]
async fn a_token_goes_over_oauthbearer_and_its_error_challenge_is_acknowledged() {
    let challenge = b"{22}\r\neyJzdGF0dXMiOiI0MDEifQ\r\nNO \"token expired\"\r\n";
    let (mut session, recorded) = greeted(fixture!("stalwart_greeting_tls"), challenge).await;
    let token = Credential::bearer_for("alice@example.com", "ya29.secret");
    let err = session
        .authenticate(&token, HOST, Some(4190))
        .await
        .unwrap_err();
    assert!(err.is_sign_in_refusal());
    assert!(
        !err.to_string().contains("expired") && !err.to_string().contains("401"),
        "{err}"
    );
    let sent = written(&recorded);
    assert!(
        sent.starts_with("AUTHENTICATE \"OAUTHBEARER\" \""),
        "{sent}"
    );
    assert!(
        sent.ends_with("\"\r\n\"AQ==\"\r\n"),
        "the RFC 7628 acknowledgement: {sent}"
    );
}

#[tokio::test]
async fn a_token_signs_in_over_xoauth2_where_that_is_all_there_is() {
    let greeting = b"\"SASL\" \"XOAUTH2\"\r\nOK\r\n";
    let (mut session, recorded) = greeted(greeting, b"\"e30=\"\r\nOK\r\n").await;
    let token = Credential::bearer_for("alice@example.com", "ya29.secret");
    session.authenticate(&token, HOST, None).await.unwrap();
    let sent = written(&recorded);
    assert!(sent.starts_with("AUTHENTICATE \"XOAUTH2\" \""), "{sent}");
    assert!(
        sent.ends_with("\"\r\n\"\"\r\n"),
        "XOAUTH2's empty acknowledgement"
    );
}

#[tokio::test]
async fn an_answer_that_judges_no_credential_asks_for_no_other() {
    let greeting = fixture!("stalwart_greeting_tls");
    for (answer, class) in [
        (
            b"NO (TRYLATER) \"busy\"\r\n".as_slice(),
            FailureClass::Retryable,
        ),
        (
            b"NO (TRANSITION-NEEDED) \"x\"\r\n",
            FailureClass::Authentication,
        ),
        (
            b"NO (AUTH-TOO-WEAK) \"x\"\r\n",
            FailureClass::Authentication,
        ),
        (
            b"NO (ENCRYPT-NEEDED) \"x\"\r\n",
            FailureClass::Authentication,
        ),
        (b"BYE \"x\"\r\n", FailureClass::Retryable),
    ] {
        let (mut session, _) = greeted(greeting, answer).await;
        let err = session
            .authenticate(&password(), HOST, None)
            .await
            .unwrap_err();
        assert!(!err.is_sign_in_refusal(), "{err:?}");
        assert_eq!(ProviderError::from(err).class(), class);
    }
}

#[tokio::test]
async fn a_server_that_will_not_stop_challenging_is_abandoned() {
    let mut answers = Vec::new();
    for _ in 0..MAX_CHALLENGES {
        answers.extend_from_slice(b"\"\"\r\n");
    }
    let (mut session, recorded) = greeted(fixture!("stalwart_greeting_tls"), &answers).await;
    let err = session
        .authenticate(&password(), HOST, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SieveError::Imap(ImapError::Protocol(_))));
    // `PLAIN` takes no challenge, so each is cancelled.
    assert_eq!(
        written(&recorded).matches("\"*\"\r\n").count(),
        MAX_CHALLENGES
    );

    let (mut session, _) = greeted(fixture!("stalwart_greeting_tls"), b"ATOM\r\n").await;
    let err = session
        .authenticate(&password(), HOST, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SieveError::Imap(ImapError::Protocol(_))));
}

#[tokio::test]
async fn a_credential_no_mechanism_can_carry_is_refused_before_sending() {
    let (mut session, recorded) = greeted(fixture!("stalwart_greeting_tls"), b"").await;
    let nul = Credential::password("alice@example.com", "a\0b");
    let err = session.authenticate(&nul, HOST, None).await.unwrap_err();
    assert!(matches!(err, SieveError::Imap(ImapError::Bad(_))));
    let anonymous = Credential::bearer("ya29.secret");
    let err = session
        .authenticate(&anonymous, HOST, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SieveError::Imap(ImapError::Bad(_))));
    assert_eq!(written(&recorded), "");
}
