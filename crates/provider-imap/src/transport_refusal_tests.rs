//! Offline tests for how a tagged `NO` is classed by its RFC 5530 response code, over a
//! scripted mock stream.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;

use super::*;
use crate::mock::{MockStream, script};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] ready\r\n";

/// The refusal a `SELECT` answered with `completion` reaches a caller as.
async fn select_refused(completion: &str) -> ImapError {
    let tagged = format!("a2 NO {completion}\r\n");
    let (stream, _) = MockStream::new(script(&[GREETING, "a1 OK LOGIN ok\r\n", &tagged]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice@test.local", "pw").await.unwrap();
    conn.select("INBOX").await.unwrap_err()
}

#[tokio::test]
async fn a_refusal_the_server_calls_temporary_may_be_asked_again() {
    for completion in [
        "[UNAVAILABLE] Backend down for maintenance",
        "[INUSE] Mailbox in use by another session",
        "[SERVERBUG] Internal error",
        "[unavailable] lower case is the same atom",
    ] {
        let err = select_refused(completion).await;
        assert!(matches!(err, ImapError::No(_)), "{completion}: {err:?}");
        assert_eq!(
            ProviderError::from(err).class(),
            FailureClass::Retryable,
            "{completion}"
        );
    }
}

#[tokio::test]
async fn a_refusal_without_a_temporary_code_is_the_state_it_names() {
    for completion in [
        "[NONEXISTENT] No such mailbox",
        "[NOPERM] Access denied",
        "[CANNOT] Not possible on this server",
        "[ALREADYEXISTS] Mailbox exists",
        "Mailbox does not exist",
        // A code is only a code at the start of the text.
        "Refused, [UNAVAILABLE] appears only in the prose",
        // Any implementation limit, such as too many keywords: no wait fixes it. Only a
        // sign-in reads it as the server limiting sessions.
        "[LIMIT] Too many keywords",
    ] {
        let err = select_refused(completion).await;
        assert!(matches!(err, ImapError::No(_)), "{completion}: {err:?}");
        assert_eq!(
            ProviderError::from(err).class(),
            FailureClass::InvalidState,
            "{completion}"
        );
    }
}

#[tokio::test]
async fn a_login_refused_while_a_backend_is_down_is_not_a_bad_password() {
    // RFC 5530 §3 gives `UNAVAILABLE` at `LOGIN` as its example: the directory that checks
    // passwords is down, and the password was never judged.
    for refusal in [
        "a1 NO [UNAVAILABLE] User's backend down for maintenance\r\n",
        "a1 NO [SERVERBUG] Internal error\r\n",
    ] {
        let (stream, _) = MockStream::new(script(&[GREETING, refusal]));
        let mut conn = Connection::open(stream).await.unwrap();

        let err = conn.login("alice@test.local", "pw").await.unwrap_err();
        assert!(!matches!(err, ImapError::Auth(_)), "{refusal}: {err:?}");
        assert_eq!(err.failure_class(), FailureClass::Retryable, "{refusal}");
    }
}
