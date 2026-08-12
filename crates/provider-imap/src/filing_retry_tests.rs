//! The repair: a Sent copy that cannot be filed over the provider's standing IMAP session
//! is filed over a freshly dialed one.
//!
//! This needs a **real dial**, so it runs against in-process TLS servers rather than the
//! `MockStream` the rest of the filing tests use — the whole point is what happens when the
//! session the provider holds is dead and another has to be opened, which a mock stream
//! cannot express.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use engine_core::{
    ids::{MailboxId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::Draft;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer},
};

use crate::{ImapProvider, config::ImapConfig};

/// A self-signed cert and the acceptor presenting it, plus a connector trusting only it.
fn tls_pair() -> (
    engine_tls::CertificateDer<'static>,
    TlsAcceptor,
    tokio_rustls::TlsConnector,
) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    let config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(tokio_rustls::rustls::DEFAULT_VERSIONS)
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .expect("server cert/key");
    let connector = engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert.clone()]))
        .expect("client config")
        .connector();
    (cert, TlsAcceptor::from(Arc::new(config)), connector)
}

/// What one accepted IMAP session should do after login.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionScript {
    /// Dial and negotiate normally, then hang up the moment the session is *used* to file —
    /// a connection that was healthy when it was opened and died while idle since, which is
    /// the only way the client ever finds out.
    DieOnFiling,
    /// Serve the filing conversation: `LIST` → `SELECT` → `UID SEARCH` → `APPEND`.
    ServeFiling,
    /// Serve the filing conversation, but answer the probe with an already-present copy, so
    /// the retry must **not** append a second one.
    ServeFilingWithCopyPresent,
}

/// Runs one IMAP session over an established TLS stream, replying per `script`. Replies are
/// tagged from the client's own tag, so the exact command order the client picks does not
/// have to be predicted here.
async fn serve_imap<S>(stream: S, script: SessionScript, appends: &AtomicUsize)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BufReader::new(stream);
    buf.write_all(b"* OK ready\r\n").await.expect("greeting");
    buf.flush().await.expect("flush greeting");
    let mut logged_in = false;
    loop {
        let mut line = String::new();
        if buf.read_line(&mut line).await.expect("read command") == 0 {
            return;
        }
        let tag = line.split_whitespace().next().unwrap_or("*").to_owned();
        let upper = line.to_ascii_uppercase();
        let command = upper.split_whitespace().nth(1).unwrap_or("").to_owned();
        if script == SessionScript::DieOnFiling && logged_in && command == "LIST" {
            // The stale session: the socket is gone, and the client discovers it on the
            // first command it sends after the idle gap.
            return;
        }
        let reply = match command.as_str() {
            "LOGIN" => {
                logged_in = true;
                format!("{tag} OK LOGIN completed\r\n")
            }
            "CAPABILITY" => {
                format!("* CAPABILITY IMAP4rev1 UIDPLUS\r\n{tag} OK CAPABILITY done\r\n")
            }
            "LIST" => {
                format!("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n{tag} OK LIST done\r\n")
            }
            "SELECT" => format!(
                "* 3 EXISTS\r\n* OK [UIDVALIDITY 99] valid\r\n{tag} OK [READ-WRITE] SELECT done\r\n"
            ),
            "UID" if upper.contains("SEARCH") => {
                // A real server answers with what it holds, so anything this fake already
                // accepted is found by a later probe — including across sessions, which is
                // what makes "press the repair twice" behave as it does in the field.
                if script == SessionScript::ServeFilingWithCopyPresent {
                    format!("* SEARCH 12\r\n{tag} OK SEARCH done\r\n")
                } else if appends.load(Ordering::SeqCst) > 0 {
                    format!("* SEARCH 5\r\n{tag} OK SEARCH done\r\n")
                } else {
                    format!("* SEARCH\r\n{tag} OK SEARCH done\r\n")
                }
            }
            "APPEND" => {
                appends.fetch_add(1, Ordering::SeqCst);
                consume_literal(&mut buf, &line).await;
                format!("{tag} OK [APPENDUID 99 5] APPEND completed\r\n")
            }
            "LOGOUT" => {
                let _ = buf
                    .write_all(format!("{tag} OK LOGOUT\r\n").as_bytes())
                    .await;
                return;
            }
            _ => format!("{tag} OK done\r\n"),
        };
        buf.write_all(reply.as_bytes()).await.expect("reply");
        buf.flush().await.expect("flush reply");
    }
}

/// Answers the `APPEND` continuation and drains the `{N}` literal plus its trailing CRLF.
async fn consume_literal<S>(buf: &mut BufReader<S>, header: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let size: usize = header
        .rsplit_once('{')
        .and_then(|(_, tail)| tail.split('}').next())
        .and_then(|digits| digits.parse().ok())
        .expect("an APPEND carries a literal size");
    buf.write_all(b"+ go ahead\r\n")
        .await
        .expect("continuation");
    buf.flush().await.expect("flush continuation");
    let mut remaining = size + 2; // the literal, then the CRLF that closes the command
    let mut scratch = vec![0u8; 4096];
    while remaining > 0 {
        let take = remaining.min(scratch.len());
        let read = tokio::io::AsyncReadExt::read(buf, &mut scratch[..take])
            .await
            .expect("read literal");
        if read == 0 {
            return;
        }
        remaining -= read;
    }
}

/// An IMAP server that serves each accepted connection with the next script in `scripts`,
/// returning its port and the count of `APPEND`s it saw.
fn imap_server(
    acceptor: TlsAcceptor,
    listener: TcpListener,
    scripts: Vec<SessionScript>,
) -> Arc<AtomicUsize> {
    let appends = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&appends);
    tokio::spawn(async move {
        for script in scripts {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                serve_imap(tls, script, &counter).await;
            });
        }
    });
    appends
}

/// A minimal SMTP server that accepts one submission and reports it delivered.
async fn smtp_server(acceptor: TlsAcceptor) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind smtp");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept smtp");
        let tls = acceptor.accept(tcp).await.expect("smtp handshake");
        let mut buf = BufReader::new(tls);
        buf.write_all(b"220 mail ESMTP ready\r\n")
            .await
            .expect("greeting");
        buf.flush().await.expect("flush greeting");
        loop {
            let mut line = String::new();
            if buf.read_line(&mut line).await.expect("read") == 0 {
                return;
            }
            let upper = line.to_ascii_uppercase();
            let reply: &[u8] = if upper.starts_with("EHLO") {
                b"250-mail\r\n250 AUTH PLAIN\r\n"
            } else if upper.starts_with("AUTH") {
                b"235 2.7.0 ok\r\n"
            } else if upper.starts_with("MAIL") || upper.starts_with("RCPT") {
                b"250 2.1.0 OK\r\n"
            } else if upper.starts_with("DATA") {
                buf.write_all(b"354 go ahead\r\n").await.expect("data");
                buf.flush().await.expect("flush");
                loop {
                    let mut data = String::new();
                    let read = buf.read_line(&mut data).await.expect("read data");
                    if read == 0 || data == ".\r\n" {
                        break;
                    }
                }
                b"250 2.0.0 queued\r\n"
            } else if upper.starts_with("QUIT") {
                let _ = buf.write_all(b"221 2.0.0 bye\r\n").await;
                return;
            } else {
                continue;
            };
            buf.write_all(reply).await.expect("reply");
            buf.flush().await.expect("flush");
        }
    });
    port
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("retry-filing@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

/// Connects a provider whose IMAP sessions come from `scripts` (one per accepted dial) and
/// whose SMTP delivers cleanly. Returns the provider and the server's `APPEND` count.
async fn provider_over(
    scripts: Vec<SessionScript>,
) -> (
    ImapProvider<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    Arc<AtomicUsize>,
) {
    let (_, acceptor, connector) = tls_pair();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind imap");
    let imap_port = listener.local_addr().expect("addr").port();
    let appends = imap_server(acceptor.clone(), listener, scripts);
    let smtp_port = smtp_server(acceptor).await;
    let config = ImapConfig::new(
        format!("127.0.0.1:{imap_port}"),
        "127.0.0.1",
        "alice@test.local",
        "pw",
    )
    .with_smtp_tls(format!("127.0.0.1:{smtp_port}"), "127.0.0.1");
    let provider = ImapProvider::connect(
        &config,
        connector,
        MailboxId::try_from("INBOX").expect("mailbox"),
    )
    .await
    .expect("connect");
    (provider, appends)
}

/// The fix, end to end: the standing session is dead by the time the send needs it, and the
/// Sent copy still lands — on a session dialed for the purpose.
#[tokio::test]
async fn a_dead_standing_session_does_not_lose_the_sent_copy() {
    let (provider, appends) =
        provider_over(vec![SessionScript::DieOnFiling, SessionScript::ServeFiling]).await;

    let receipt = provider.submit(&draft()).await.expect("the send delivers");

    assert!(
        receipt.sent_copy.is_filed(),
        "the retry filed it: {:?}",
        receipt.sent_copy
    );
    assert_eq!(receipt.email_key.as_str(), "imap:v99:u5@Sent");
    assert_eq!(appends.load(Ordering::SeqCst), 1, "exactly one APPEND");
}

/// The retry must not duplicate. If the first attempt actually committed and only its
/// response was lost, the probe finds the copy and the retry appends nothing.
#[tokio::test]
async fn a_retry_never_files_a_second_copy() {
    let (provider, appends) = provider_over(vec![
        SessionScript::DieOnFiling,
        SessionScript::ServeFilingWithCopyPresent,
    ])
    .await;

    let receipt = provider.submit(&draft()).await.expect("the send delivers");

    assert!(receipt.sent_copy.is_filed());
    assert_eq!(
        receipt.email_key.as_str(),
        "imap:v99:u12@Sent",
        "the receipt points at the copy that was already there"
    );
    assert_eq!(
        appends.load(Ordering::SeqCst),
        0,
        "the probe found the copy, so nothing was appended"
    );
}

/// The repair a host runs when the user asks to try filing again: it files the copy, and
/// sends nothing.
#[tokio::test]
async fn the_explicit_repair_files_the_copy_without_sending() {
    let (provider, appends) = provider_over(vec![SessionScript::ServeFiling]).await;

    let key = provider.refile(&draft()).await.expect("the copy is filed");

    assert_eq!(key.as_str(), "imap:v99:u5@Sent");
    assert_eq!(appends.load(Ordering::SeqCst), 1);
}

/// The repair sits behind a button, so it will be pressed more than once. Every attempt
/// probes first, so a second press finds the copy and leaves Sent holding exactly one.
#[tokio::test]
async fn pressing_the_repair_twice_leaves_one_copy() {
    let (provider, appends) = provider_over(vec![SessionScript::ServeFiling]).await;
    let draft = draft();

    let first = provider
        .refile(&draft)
        .await
        .expect("first repair files it");
    assert_eq!(appends.load(Ordering::SeqCst), 1);

    let second = provider
        .refile(&draft)
        .await
        .expect("second repair is a no-op");

    assert_eq!(first, second, "both presses name the same one copy");
    assert_eq!(
        appends.load(Ordering::SeqCst),
        1,
        "the second press appended nothing"
    );
}

/// When the retry cannot reach the server either, the send still succeeds and the receipt
/// carries the reason — the shape that lets the host tell the user rather than lose it.
#[tokio::test]
async fn a_send_survives_a_failed_retry_and_reports_why() {
    let (provider, _) = provider_over(vec![SessionScript::DieOnFiling]).await;

    let receipt = provider
        .submit(&draft())
        .await
        .expect("a delivered send is never failed for a filing error");

    let detail = receipt
        .sent_copy
        .unfiled_detail()
        .expect("an unfiled copy carries why");
    assert!(
        detail.contains("retry on a fresh session"),
        "both attempts are in the detail: {detail}"
    );
}
