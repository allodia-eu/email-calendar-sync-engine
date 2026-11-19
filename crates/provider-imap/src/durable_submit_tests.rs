//! A send through the outbox and a real SMTP conversation, cut off at every point of it.
//!
//! The server counts a message the moment it reads the `.` that ends it, as a real one queues
//! it there, and reports each point of the conversation as it reaches it. "The process ends"
//! is the attempt's future being dropped when the server reports a point: the socket closes and
//! nothing the attempt would have written afterwards is written, which is what the server and
//! the store see of a process that was killed. Recovery is the next drain once the lease has
//! lapsed, against the same server.
//!
//! Every case ends with the recipient holding the message exactly once, or none and the send
//! awaiting confirmation; never twice.

use core::time::Duration;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::EmailAddress,
    write::{IdempotencyKey, PendingOp, PendingOpKind, ResourceKey},
};
use engine_provider::{Draft, HandOver, HandOverLog, Provider};
use engine_store::{
    Confirmation, LeaseRequest, ManualClock, OpLease, PendingOpClaim, PendingOpState, Store,
    StoreRead, WorkerId,
};
use engine_sync::{drain_outbox, submit_mail};
use store_sqlite::SqliteStore;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};

use super::smtp_server_tests::{cert_and_acceptor, provider_with_smtp, trusting_connector};
use crate::{ImapProvider, config::ImapConfig, filing::resolve_smtp, mock::MockStream};

/// A point of the conversation the server has reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    /// The TCP connection is up; the TLS handshake has not been answered.
    Accepted,
    /// The greeting went out and `EHLO` came in.
    Ehlo,
    /// `AUTH` came in.
    Auth,
    /// The envelope is in: `RCPT` came in.
    Rcpt,
    /// The first line of the message came in.
    Body,
    /// The `.` that ends the message came in: queued.
    Dot,
    /// The `250` for it went out.
    Replied,
}

/// What the server does once it has the message's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterDot {
    Accept,
    Silent,
    /// Queued, and the connection closes before the reply.
    Drop,
    Defer,
    Reject,
}

struct Server {
    port: u16,
    cert: engine_tls::CertificateDer<'static>,
    delivered: Arc<AtomicUsize>,
    points: mpsc::UnboundedReceiver<Point>,
}

impl Server {
    /// Serves one connection per entry of `script`, then accepts every further message.
    async fn start(script: Vec<AfterDot>) -> Self {
        let (cert, acceptor) = cert_and_acceptor();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let delivered = Arc::new(AtomicUsize::new(0));
        let (tx, points) = mpsc::unbounded_channel();
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let count = Arc::clone(&delivered);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let _ = tx.send(Point::Accepted);
                let after_dot = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(AfterDot::Accept);
                let (acceptor, tx, count) = (acceptor.clone(), tx.clone(), Arc::clone(&count));
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let _ = converse(tls, &tx, &count, after_dot).await;
                });
            }
        });
        Self {
            port,
            cert,
            delivered,
            points,
        }
    }

    fn delivered(&self) -> usize {
        self.delivered.load(Ordering::SeqCst)
    }

    fn provider(&self) -> ImapProvider<MockStream> {
        let config = ImapConfig::new(
            "h:993",
            "127.0.0.1",
            crate::credentials::Credentials::password("alice", "pw"),
        )
        .with_smtp_tls(format!("127.0.0.1:{}", self.port), "127.0.0.1");
        let connector = trusting_connector(self.cert.clone());
        provider_with_smtp(resolve_smtp(
            config.smtp.as_ref().unwrap(),
            &connector,
            &config,
        ))
    }

    /// Waits until the server reaches `point`.
    async fn reaches(&mut self, point: Point) {
        while let Some(reached) = self.points.recv().await {
            if reached == point {
                return;
            }
        }
        panic!("the server stopped before {point:?}");
    }
}

/// One SMTP session. Each point is reported before the reply that lets the client past it,
/// so a client cut off there has not been told anything more.
async fn converse(
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    tx: &mpsc::UnboundedSender<Point>,
    delivered: &AtomicUsize,
    after_dot: AfterDot,
) -> std::io::Result<()> {
    let mut io = BufReader::new(stream);
    io.write_all(b"220 mail ESMTP ready\r\n").await?;
    io.flush().await?;
    loop {
        let mut line = String::new();
        if io.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            let _ = tx.send(Point::Ehlo);
            b"250-mail\r\n250 AUTH PLAIN\r\n"
        } else if upper.starts_with("AUTH") {
            let _ = tx.send(Point::Auth);
            b"235 2.7.0 ok\r\n"
        } else if upper.starts_with("MAIL") {
            b"250 2.1.0 ok\r\n"
        } else if upper.starts_with("RCPT") {
            let _ = tx.send(Point::Rcpt);
            b"250 2.1.5 ok\r\n"
        } else if upper.starts_with("DATA") {
            io.write_all(b"354 go ahead\r\n").await?;
            io.flush().await?;
            let mut first = true;
            loop {
                let mut data = String::new();
                if io.read_line(&mut data).await? == 0 {
                    // Closed before the end: nothing is queued.
                    return Ok(());
                }
                if data == ".\r\n" {
                    break;
                }
                if first {
                    let _ = tx.send(Point::Body);
                    first = false;
                }
            }
            match after_dot {
                AfterDot::Accept | AfterDot::Silent | AfterDot::Drop => {
                    delivered.fetch_add(1, Ordering::SeqCst);
                }
                AfterDot::Defer | AfterDot::Reject => {}
            }
            let _ = tx.send(Point::Dot);
            match after_dot {
                AfterDot::Accept => {
                    io.write_all(b"250 2.0.0 queued\r\n").await?;
                    io.flush().await?;
                    let _ = tx.send(Point::Replied);
                    continue;
                }
                AfterDot::Silent => return std::future::pending().await,
                AfterDot::Drop => return Ok(()),
                AfterDot::Defer => b"451 4.3.0 try later\r\n",
                AfterDot::Reject => b"554 5.7.1 refused\r\n",
            }
        } else if upper.starts_with("QUIT") {
            let _ = io.write_all(b"221 bye\r\n").await;
            return Ok(());
        } else {
            continue;
        };
        io.write_all(reply).await?;
        io.flush().await?;
    }
}

const TTL: Duration = Duration::from_mins(1);

fn account() -> AccountId {
    AccountId::try_from("acct-durable").unwrap()
}

fn draft(id: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body text",
    )
}

fn store() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap());
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

async fn state(store: &SqliteStore<ManualClock>) -> PendingOpState {
    let rows = store.list_pending_ops(account()).await.unwrap();
    rows.first()
        .map_or(PendingOpState::Succeeded, |row| row.state)
}

/// The next process: the lease has lapsed, and a drain runs against the same server.
async fn recover(server: &Server, store: &SqliteStore<ManualClock>, clock: &ManualClock) {
    clock.advance(TTL * 2);
    drain_outbox(
        &server.provider(),
        store,
        &account(),
        WorkerId::new("next"),
        TTL,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_send_cut_off_anywhere_before_its_end_reaches_the_recipient_once() {
    for point in [
        Point::Accepted,
        Point::Ehlo,
        Point::Auth,
        Point::Rcpt,
        Point::Body,
    ] {
        let mut server = Server::start(vec![]).await;
        let (store, clock) = store();
        let provider = server.provider();
        let mut message = draft("cut-early@test.local");
        if point == Point::Body {
            // Larger than the socket buffers hold, so the client is still writing it when the
            // server has read its first line.
            message.text_body = "a line of the message body\r\n".repeat(400_000);
        }
        let acct = account();
        tokio::select! {
            biased;
            () = server.reaches(point) => {}
            sent = submit_mail(&provider, &store, &acct, WorkerId::new("w"), TTL, &message) => {
                panic!("{point:?}: meant to be cut off, ended {sent:?}");
            }
        }
        assert_eq!(server.delivered(), 0, "{point:?}");

        recover(&server, &store, &clock).await;
        assert_eq!(
            server.delivered(),
            1,
            "{point:?}: sent once by the next process"
        );
        assert_eq!(state(&store).await, PendingOpState::Succeeded, "{point:?}");
    }
}

#[tokio::test]
async fn a_send_cut_off_after_its_end_left_is_never_sent_again() {
    for (point, after_dot) in [
        (Point::Dot, AfterDot::Silent),
        (Point::Replied, AfterDot::Accept),
    ] {
        let mut server = Server::start(vec![after_dot]).await;
        let (store, clock) = store();
        let provider = server.provider();
        let (message, acct) = (draft("cut-late@test.local"), account());
        tokio::select! {
            biased;
            () = server.reaches(point) => {}
            sent = submit_mail(&provider, &store, &acct, WorkerId::new("w"), TTL, &message) => {
                panic!("{point:?}: meant to be cut off, ended {sent:?}");
            }
        }
        assert_eq!(server.delivered(), 1, "{point:?}");

        recover(&server, &store, &clock).await;
        assert_eq!(server.delivered(), 1, "{point:?}: never a second copy");
        assert_eq!(
            state(&store).await,
            PendingOpState::NeedsConfirmation,
            "{point:?}"
        );
    }
}

/// A log that records the hand-over in the store and then, as told, lets the attempt go on,
/// stops it for good (the process ends right there), or fails to record at all.
struct Log<'a> {
    store: &'a SqliteStore<ManualClock>,
    lease: &'a OpLease,
    then: Then,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Then {
    DieBefore,
    DieAfter,
    Refuse,
}

#[async_trait::async_trait]
impl HandOverLog for Log<'_> {
    async fn record_hand_over(&self) -> Result<(), String> {
        match self.then {
            Then::DieBefore => std::future::pending().await,
            Then::Refuse => Err("disk full".to_owned()),
            Then::DieAfter => {
                self.store
                    .record_hand_over(self.lease)
                    .await
                    .map_err(|err| err.to_string())?;
                std::future::pending().await
            }
        }
    }
}

/// The attempt the outbox makes, with the hand-over going through `then`.
async fn attempt_with(
    server: &Server,
    store: &SqliteStore<ManualClock>,
    message: &Draft,
    then: Then,
) -> Option<engine_provider::ProviderError> {
    let id = message.message_id.as_str();
    let op = PendingOp::new(
        IdempotencyKey::new(format!("submit:{id}")).unwrap(),
        PendingOpKind::MailSubmit,
        ResourceKey::new(format!("draft:{id}")).unwrap(),
        serde_json::to_value(message).unwrap(),
    );
    let op = store.enqueue_pending_op(account(), op).await.unwrap();
    let request = LeaseRequest::new(WorkerId::new("w"), TTL);
    let PendingOpClaim::Leased(leased) = store
        .claim_pending_op(account(), op, request)
        .await
        .unwrap()
    else {
        panic!("claimable");
    };
    let log = Log {
        store,
        lease: &leased.lease,
        then,
    };
    let hand_over = HandOver::new(&log);
    let sending = server
        .provider()
        .submit_email(&account(), message, &hand_over)
        .await;
    sending.err()
}

#[tokio::test]
async fn a_send_cut_off_with_its_whole_body_written_but_no_hand_over_goes_out_once() {
    let server = Server::start(vec![]).await;
    let (store, clock) = store();
    let message = draft("whole-body@test.local");
    let cut = tokio::time::timeout(
        Duration::from_secs(2),
        attempt_with(&server, &store, &message, Then::DieBefore),
    )
    .await;
    assert!(cut.is_err(), "the attempt stopped at its hand-over");
    assert_eq!(server.delivered(), 0, "the end of the message never left");

    recover(&server, &store, &clock).await;
    assert_eq!(server.delivered(), 1);
    assert_eq!(state(&store).await, PendingOpState::Succeeded);
}

/// Cut off between recording the hand-over and the `.`: the server never had the end, but
/// nothing can know that, so the send awaits confirmation rather than risking a second copy,
/// and the user's "it did not arrive" sends it once.
#[tokio::test]
async fn a_send_cut_off_between_its_hand_over_and_its_end_asks_and_is_never_doubled() {
    let server = Server::start(vec![]).await;
    let (store, clock) = store();
    let message = draft("marked-only@test.local");
    let cut = tokio::time::timeout(
        Duration::from_secs(2),
        attempt_with(&server, &store, &message, Then::DieAfter),
    )
    .await;
    assert!(cut.is_err());
    assert_eq!(server.delivered(), 0);

    recover(&server, &store, &clock).await;
    assert_eq!(
        server.delivered(),
        0,
        "possibly delivered is never resent unasked"
    );
    let op = store.list_pending_ops(account()).await.unwrap()[0].id;
    assert_eq!(state(&store).await, PendingOpState::NeedsConfirmation);
    assert_eq!(
        store
            .confirm_pending_op(account(), op, Confirmation::NotDelivered)
            .await
            .unwrap(),
        None
    );
    recover(&server, &store, &clock).await;
    assert_eq!(server.delivered(), 1);
}

/// A hand-over the outbox cannot record stops the attempt before the `.`: retried, and sent
/// once by the next drain.
#[tokio::test]
async fn a_hand_over_that_cannot_be_recorded_never_ends_the_message() {
    let server = Server::start(vec![]).await;
    let (store, clock) = store();
    let message = draft("unrecorded@test.local");
    let err = attempt_with(&server, &store, &message, Then::Refuse)
        .await
        .expect("the attempt failed");
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    assert_eq!(server.delivered(), 0);
    // The attempt that refused recorded nothing; its lease lapsing is the dead attempt.
    recover(&server, &store, &clock).await;
    assert_eq!(server.delivered(), 1);
}

/// The server's own answer to the end of the message is definitive either way: a deferral is
/// retried and sent once, a refusal settles and stays listed, and a dropped connection after
/// the end awaits confirmation.
#[tokio::test]
async fn the_servers_answer_to_the_end_decides_and_its_silence_asks() {
    for (after_dot, settled, delivered) in [
        (AfterDot::Defer, PendingOpState::Succeeded, 1),
        (AfterDot::Reject, PendingOpState::Failed, 0),
        (AfterDot::Drop, PendingOpState::NeedsConfirmation, 1),
    ] {
        let server = Server::start(vec![after_dot]).await;
        let (store, clock) = store();
        let (message, acct) = (draft("answered@test.local"), account());
        let provider = server.provider();
        let sent = submit_mail(&provider, &store, &acct, WorkerId::new("w"), TTL, &message);
        assert!(sent.await.is_err(), "{after_dot:?}");
        recover(&server, &store, &clock).await;
        assert_eq!(state(&store).await, settled, "{after_dot:?}");
        assert_eq!(server.delivered(), delivered, "{after_dot:?}");
    }
}
