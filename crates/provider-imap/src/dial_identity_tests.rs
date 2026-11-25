//! Offline tests for the one identity an account's connections sign in as: once a dial has
//! signed in, a credential naming another account fails its dial before anything is opened, so
//! a source that answers for the wrong account cannot put that account's session in the pool
//! (`docs/design/credential-source.md`, "Direct host").

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, ProviderError,
};

use super::{
    dial_with,
    renewal_tests::{GREETING, Servers, token_config},
};
use crate::{credentials::PresentedIdentity, error::ImapError, mock::script, pool::ImapPool};

const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";
const MASTER: &str = "master@example.com";
/// base64 of `alice@example.com\0master@example.com\0master-pw` (RFC 4616).
const ACTING_AS_ALICE: &str = "YWxpY2VAZXhhbXBsZS5jb20AbWFzdGVyQGV4YW1wbGUuY29tAG1hc3Rlci1wdw==";

/// A source that answers each ask, `credential` or `replace`, with the next of `answers`.
#[derive(Debug)]
struct Answers {
    subject: CredentialSubject,
    answers: Mutex<VecDeque<Credential>>,
}

impl Answers {
    fn new(answers: Vec<Credential>) -> Arc<Self> {
        Arc::new(Self {
            subject: CredentialSubject::new("one-account"),
            answers: Mutex::new(answers.into()),
        })
    }

    fn next(&self) -> Credential {
        let next = self.answers.lock().unwrap().pop_front();
        next.expect("the dial asked more often than the test scripted")
    }
}

#[async_trait]
impl CredentialSource for Answers {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Ok(self.next())
    }

    async fn replace(
        &self,
        _refused: &Credential,
        _request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        Ok(Some(self.next()))
    }
}

/// A server that accepts a `LOGIN`.
fn accepting_login() -> Vec<u8> {
    script(&[
        GREETING,
        "a1 OK logged in\r\n",
        "* CAPABILITY IMAP4rev1 IDLE\r\na2 OK done\r\n",
    ])
}

/// A server that refuses a `LOGIN`.
fn refusing_login() -> Vec<u8> {
    script(&[
        GREETING,
        "a1 NO [AUTHENTICATIONFAILED] Authentication failed.\r\n",
    ])
}

/// A server that refuses an `AUTHENTICATE PLAIN`, as Dovecot does a wrong master password.
fn refusing_plain() -> Vec<u8> {
    script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN\r\na1 OK done\r\n",
        "a2 NO [AUTHENTICATIONFAILED] Authentication failed.\r\n",
    ])
}

/// A server that accepts an `AUTHENTICATE PLAIN`.
fn accepting_plain() -> Vec<u8> {
    script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN\r\na1 OK done\r\n",
        "a2 OK Logged in\r\n",
        "* CAPABILITY IMAP4rev1 IDLE\r\na3 OK done\r\n",
    ])
}

fn is_another_account(err: &ImapError) -> bool {
    matches!(err, ImapError::Credential(_)) && err.failure_class() == FailureClass::InvalidState
}

#[tokio::test]
async fn a_pool_refuses_a_dial_for_another_account_and_opens_nothing_for_it() {
    let source = Answers::new(vec![
        Credential::password(ALICE, "pw"),
        Credential::password(BOB, "pw"),
    ]);
    let servers = Servers::new(vec![accepting_login(), accepting_login()]);
    let dial = servers.dial(token_config(source));
    let pool = ImapPool::new(dial, 5, Duration::from_hours(1));

    drop(pool.acquire().await.expect("the first dial signs in"));
    // Drop the parked connection, so the next caller dials; the source answers for bob.
    pool.invalidate();
    let Err(err) = pool.acquire().await else {
        panic!("a connection signed in as another account joined the pool");
    };

    assert!(is_another_account(&err), "{err:?}");
    assert_eq!(servers.opened(), 1, "nothing is opened for another account");
    assert!(!err.to_string().contains("example.com"), "{err}");
    // The source's mistake, not the server's: the pooled connections are sound.
    assert!(ProviderError::from(err).is_credential_failure());
}

#[tokio::test]
async fn acting_as_an_account_is_that_accounts_identity_not_the_administrators() {
    let source = Answers::new(vec![
        Credential::acting_as(MASTER, "master-pw", ALICE),
        Credential::password(ALICE, "pw"),
        Credential::acting_as(MASTER, "master-pw", BOB),
        Credential::password(MASTER, "master-pw"),
    ]);
    let servers = Servers::new(vec![accepting_plain(), accepting_login()]);
    let config = token_config(source);
    let presented = PresentedIdentity::default();
    let dial = || dial_with(&config, &presented, || servers.open());

    dial()
        .await
        .expect("the administrator signs in acting as alice");
    // On the wire: `AUTHENTICATE PLAIN` naming alice as the authorization identity, never a
    // `LOGIN` as the administrator, which would open the administrator's own mailbox.
    let wire = servers.written(0);
    assert!(
        wire.contains(&format!("a2 AUTHENTICATE PLAIN {ACTING_AS_ALICE}\r\n")),
        "{wire}"
    );
    assert!(!wire.contains("LOGIN"), "{wire}");
    // Alice's own password opens the same mailbox, so it belongs in the same pool.
    dial().await.expect("alice herself");
    // The same administrator acting as bob, or as itself, would open another mailbox.
    for _ in 0..2 {
        let err = dial().await.expect_err("another mailbox");
        assert!(is_another_account(&err), "{err:?}");
    }
    assert_eq!(servers.opened(), 2);
}

#[tokio::test]
async fn a_replacement_naming_another_account_fails_even_on_the_first_dial() {
    // Nothing is remembered yet, so only the refused credential says which account this is:
    // a replacement for another account is the source answering for the wrong subject.
    let source = Answers::new(vec![
        Credential::acting_as(MASTER, "master-pw", ALICE),
        Credential::acting_as(MASTER, "master-pw", BOB),
    ]);
    let servers = Servers::new(vec![refusing_plain(), accepting_plain()]);
    let config = token_config(source);
    let presented = PresentedIdentity::default();

    let err = dial_with(&config, &presented, || servers.open())
        .await
        .expect_err("bob does not replace alice");

    assert!(is_another_account(&err), "{err:?}");
    assert_eq!(servers.opened(), 1, "nothing is opened for bob");
    assert_eq!(format!("{presented:?}"), "PresentedIdentity(<absent>)");
}

#[tokio::test]
async fn only_a_dial_that_signed_in_decides_the_identity() {
    // The first dial is refused twice and fails, so it decides nothing: the account the next
    // dial signs in as is the account's identity.
    let source = Answers::new(vec![
        Credential::password(ALICE, "wrong"),
        Credential::password(ALICE, "still-wrong"),
        Credential::password(BOB, "pw"),
        Credential::password(BOB, "pw"),
    ]);
    let servers = Servers::new(vec![
        refusing_login(),
        refusing_login(),
        accepting_login(),
        accepting_login(),
    ]);
    let config = token_config(source);
    let presented = PresentedIdentity::default();

    let err = dial_with(&config, &presented, || servers.open())
        .await
        .expect_err("refused, and refused again");
    assert!(matches!(err, ImapError::Auth(_)), "{err:?}");
    dial_with(&config, &presented, || servers.open())
        .await
        .expect("the first account to sign in");
    dial_with(&config, &presented, || servers.open())
        .await
        .expect("the account that signed in, again");
    assert_eq!(format!("{presented:?}"), "PresentedIdentity(<present>)");
}
