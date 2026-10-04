//! What a host is told about a submission whose server stopped answering: a failure the
//! outbox retries when nothing was submitted, and a question for the user when the message
//! may have been.
//!
//! On tokio's paused clock, under a guard that turns a missing bound into a failure rather
//! than a hang, as in `deadline_tests`.

use std::time::Duration;

use engine_core::{
    ids::{MailboxId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::{Draft, ProviderError, ProviderResult, SubmissionReceipt};
use tokio::net::TcpListener;

use crate::{
    ImapProvider,
    config::ImapConfig,
    credentials::Credentials,
    filing::resolve_smtp,
    mock::{MockStream, script},
    transport::Connection,
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("silent-server@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body text",
    )
}

/// A provider whose IMAP side is an empty mock: none of these sends gets as far as filing.
fn provider() -> ImapProvider<MockStream> {
    let (stream, _) = MockStream::new(script(&[]));
    ImapProvider::with_connection(
        Connection::resume(stream),
        MailboxId::try_from("INBOX").unwrap(),
    )
}

async fn guarded(
    submission: impl Future<Output = ProviderResult<SubmissionReceipt>>,
) -> ProviderError {
    tokio::time::timeout(GUARD, submission)
        .await
        .expect("hung: nothing bounded the wait on a silent server")
        .expect_err("a silent server does not deliver")
}

#[tokio::test(start_paused = true)]
async fn a_send_to_a_server_that_never_greets_is_retried_by_the_outbox() {
    let (smtp, _) = MockStream::silent_after(script(&[]));

    let err = guarded(provider().submit_over(smtp, &draft(), None)).await;

    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_send_whose_end_of_data_goes_unanswered_needs_confirmation_and_is_never_retried() {
    let (smtp, _) = MockStream::silent_after(script(&[
        "220 mail ESMTP\r\n",
        "250 mail\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
    ]));

    let err = guarded(provider().submit_over(smtp, &draft(), None)).await;

    assert!(err.requires_confirmation(), "{err}");
    assert!(!err.is_retryable(), "{err}");
}

#[tokio::test]
async fn a_tls_submission_to_a_server_that_accepts_and_never_speaks_is_retried() {
    // The shape a paused server behind a port forwarder takes: the connect succeeds and the
    // handshake is never answered. The listener is held open and never accepts in code; the
    // kernel completes the connection all the same.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let config = ImapConfig::new("h:993", "127.0.0.1", Credentials::password("alice", "pw"))
        .with_smtp_tls(format!("127.0.0.1:{port}"), "127.0.0.1");
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    let sender = resolve_smtp(config.smtp.as_ref().unwrap(), &connector, &config);
    let (stream, _) = MockStream::new(script(&[]));
    let provider = ImapProvider::with_connection_and_smtp(
        Connection::resume(stream),
        MailboxId::try_from("INBOX").unwrap(),
        sender,
    );
    tokio::time::pause();

    let err = guarded(provider.submit(&draft())).await;

    assert!(err.to_string().contains("TLS handshake"), "{err}");
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    drop(listener);
}
