//! Offline tests for an administrator signing in as another account: IMAP `AUTHENTICATE PLAIN`
//! with an authorization identity (RFC 4616), over a scripted mock stream.
//!
//! The server lines are Dovecot 2.4.4's, observed on the harness with a master user configured
//! (`docker/dovecot/README.md`), so the shapes pinned here are the ones a real server sent. The
//! mock still answers whatever it is sent: whether a server accepts these bytes is the gated
//! live test's job (`tests/live_imap_acting_as.rs`).

use engine_core::error::FailureClass;

use crate::{
    error::ImapError,
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1 IMAP4rev2 LOGIN-REFERRALS ID ENABLE IDLE \
                        SASL-IR LITERAL+ AUTH=PLAIN] Dovecot ready.\r\n";
/// Dovecot's pre-login `CAPABILITY` over TLS, verbatim.
const CAPABILITY: &str = "* CAPABILITY IMAP4rev1 IMAP4rev2 LOGIN-REFERRALS ID ENABLE IDLE \
                          SASL-IR LITERAL+ AUTH=PLAIN\r\n\
                          a1 OK Pre-login capabilities listed, post-login capabilities have \
                          more.\r\n";
/// The same, from a server that takes no initial response.
const CAPABILITY_WITHOUT_SASL_IR: &str = "* CAPABILITY IMAP4rev1 AUTH=PLAIN\r\na1 OK done\r\n";

/// base64 of `alice@test.local\0master@test.local\0dovecot-master-pw`.
const TOKEN: &str = "YWxpY2VAdGVzdC5sb2NhbABtYXN0ZXJAdGVzdC5sb2NhbABkb3ZlY290LW1hc3Rlci1wdw==";

/// The master user signing in as alice over `server_script`: the outcome, and what the client
/// wrote.
async fn act_as_alice(server_script: Vec<u8>) -> (Result<(), ImapError>, String) {
    let (stream, recorded) = MockStream::new(server_script);
    let mut conn = Connection::open(stream).await.expect("greeting");
    let outcome = conn
        .authenticate_acting_as("master@test.local", "dovecot-master-pw", "alice@test.local")
        .await;
    (outcome, written(&recorded))
}

#[tokio::test]
async fn the_authorization_identity_rides_one_plain_line_where_sasl_ir_is_offered() {
    let (outcome, sent) =
        act_as_alice(script(&[GREETING, CAPABILITY, "a2 OK Logged in\r\n"])).await;
    outcome.expect("signed in");

    assert_eq!(
        sent,
        format!("a1 CAPABILITY\r\na2 AUTHENTICATE PLAIN {TOKEN}\r\n")
    );
}

#[tokio::test]
async fn without_sasl_ir_the_response_follows_the_servers_prompt() {
    // Dovecot's prompt is `+ ` with a trailing space; Stalwart's is a bare `+`.
    let (outcome, sent) = act_as_alice(script(&[
        GREETING,
        CAPABILITY_WITHOUT_SASL_IR,
        "+ \r\n",
        "a2 OK Logged in\r\n",
    ]))
    .await;
    outcome.expect("signed in");

    assert_eq!(
        sent,
        format!("a1 CAPABILITY\r\na2 AUTHENTICATE PLAIN\r\n{TOKEN}\r\n")
    );
}

#[tokio::test]
async fn a_refusal_is_an_authentication_failure_as_for_login() {
    // What Dovecot answered both to a wrong master password and to an account that is not a
    // master user naming another account's mailbox.
    let (outcome, _) = act_as_alice(script(&[
        GREETING,
        CAPABILITY,
        "a2 NO [AUTHENTICATIONFAILED] Authentication failed.\r\n",
    ]))
    .await;

    let err = outcome.expect_err("refused");
    assert!(matches!(err, ImapError::Auth(_)), "{err:?}");
    assert_eq!(err.failure_class(), FailureClass::Authentication);
}

#[tokio::test]
async fn a_limit_on_sessions_is_rate_limited_not_a_wrong_password() {
    let (outcome, _) = act_as_alice(script(&[
        GREETING,
        CAPABILITY,
        "a2 NO [LIMIT] Too many sessions.\r\n",
    ]))
    .await;

    let err = outcome.expect_err("refused for now");
    assert_eq!(err.failure_class(), FailureClass::RateLimited, "{err:?}");
}

#[tokio::test]
async fn a_server_without_auth_plain_is_told_so_and_sent_nothing() {
    let (outcome, sent) = act_as_alice(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 LOGINDISABLED AUTH=XOAUTH2\r\na1 OK done\r\n",
    ]))
    .await;

    let err = outcome.expect_err("no mechanism to act as another account");
    assert!(matches!(err, ImapError::AuthUnsupported(_)), "{err:?}");
    assert!(err.to_string().contains("it offers: XOAUTH2"), "{err}");
    assert_eq!(sent, "a1 CAPABILITY\r\n");
}

#[tokio::test]
async fn a_challenge_after_the_response_is_cancelled() {
    // PLAIN defines no challenge after the credential. A server that sends one anyway is
    // answered with the cancellation, never with the credential again, and its `BAD` is a
    // refused command rather than a refused credential.
    let (outcome, sent) = act_as_alice(script(&[
        GREETING,
        CAPABILITY,
        "+ bW9yZT8=\r\n",
        "a2 BAD Authentication cancelled\r\n",
    ]))
    .await;

    let err = outcome.expect_err("cancelled");
    assert!(matches!(err, ImapError::Bad(_)), "{err:?}");
    assert!(sent.ends_with(&format!("{TOKEN}\r\n*\r\n")), "{sent}");
}
