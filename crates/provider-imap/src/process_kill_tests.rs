//! A send through the outbox in a **child process** that is killed (`SIGKILL` on Unix) at a
//! chosen point of the SMTP conversation, after which this process opens the same store file,
//! runs the start-up recovery and a drain, and counts what the server received.
//!
//! The child is this test binary run again with [`CHILD`] set, which turns
//! [`a_child_process_runs_the_outbox_when_asked`] from a no-op into the child's body. The kill
//! comes from the server side of the conversation, held at the point, so the point is exact.
//!
//! Two servers: one scripted here (every point, fine-grained), and the Stalwart harness
//! through a byte proxy (the two points that matter most on a real server), gated on the
//! harness being configured.

use core::time::Duration;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::EmailAddress,
    time::UtcDateTime,
};
use engine_provider::{Draft, Provider};
use engine_store::{Clock, PendingOpState, Store, StoreRead, WorkerId};
use engine_sync::{drain_outbox, submit_mail};
use serde::{Deserialize, Serialize};
use store_sqlite::SqliteStore;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Notify, mpsc},
};

use crate::{ImapProvider, filing::SmtpSender};

/// Set in a child's environment to the [`Spec`] it runs.
const CHILD: &str = "ENGINE_TEST_DURABLE_SEND_CHILD";
/// The test the child runs.
const ENTRY: &str = "process_kill_tests::a_child_process_runs_the_outbox_when_asked";
const TTL: Duration = Duration::from_mins(5);

/// What a child does.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Spec {
    store: String,
    smtp: String,
    /// Stalwart's IMAP address, user and password, or `None` for an IMAP side that files
    /// nothing.
    imap: Option<(String, String, String)>,
    from: String,
    message_id: String,
    /// A message larger than the socket buffers hold.
    big: bool,
    /// Drain the outbox rather than send.
    drain: bool,
}

/// The wall clock, as a host's store runs on: the child and this process share it.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UtcDateTime {
        let now = time::OffsetDateTime::now_utc();
        UtcDateTime::new(
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
        )
        .expect("a civil UTC time")
    }
}

fn account() -> AccountId {
    AccountId::try_from("acct-process").unwrap()
}

fn draft(spec: &Spec) -> Draft {
    let mut draft = Draft::new(
        MessageIdHeader::new(spec.message_id.as_str()).unwrap(),
        EmailAddress::new(spec.from.as_str()),
        vec![EmailAddress::new(spec.from.as_str())],
        "Durable send",
        "Body text",
    );
    if spec.big {
        draft.text_body = "a line of the message body\r\n".repeat(400_000);
    }
    draft
}

/// Not a test of its own: the body of a child process, and a no-op in an ordinary run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_process_runs_the_outbox_when_asked() {
    let Ok(spec) = std::env::var(CHILD) else {
        return;
    };
    let spec: Spec = serde_json::from_str(&spec).unwrap();
    let store = SqliteStore::open(&spec.store, SystemClock).unwrap();
    match &spec.imap {
        None => act(&plaintext(&spec.smtp), &store, &spec).await,
        Some((imap, user, password)) => {
            act(
                &stalwart(imap, &spec.smtp, user, password).await,
                &store,
                &spec,
            )
            .await;
        }
    }
}

async fn act<P: Provider>(provider: &P, store: &SqliteStore<SystemClock>, spec: &Spec) {
    let worker = WorkerId::new(format!("process-{}", std::process::id()));
    if spec.drain {
        drain_outbox(provider, store, &account(), worker, TTL)
            .await
            .unwrap();
    } else {
        let _ = submit_mail(provider, store, &account(), worker, TTL, &draft(spec)).await;
    }
}

/// A provider sending over plaintext SMTP to `smtp`, whose IMAP side files nothing.
fn plaintext(smtp: &str) -> ImapProvider<crate::mock::MockStream> {
    crate::filing::smtp_server_tests::provider_with_smtp(SmtpSender::Plaintext {
        addr: smtp.to_owned(),
    })
}

/// A provider on the Stalwart harness's IMAP, sending through `smtp`.
async fn stalwart(
    imap: &str,
    smtp: &str,
    user: &str,
    password: &str,
) -> ImapProvider<tokio_rustls::client::TlsStream<TcpStream>> {
    let host = imap.rsplit_once(':').map_or("localhost", |(host, _)| host);
    let config = crate::ImapConfig::new(imap, host, crate::Credentials::password(user, password))
        .with_smtp(smtp);
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    crate::ImapAccount::connect(&config, connector)
        .await
        .expect("connect to the harness")
        .provider(MailboxId::try_from("INBOX").unwrap())
}

fn spawn(spec: &Spec) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([ENTRY, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, serde_json::to_string(spec).unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the child")
}

/// Kills `child` outright: `SIGKILL` on Unix, so it runs nothing more.
fn kill(child: &mut Child) {
    child.kill().expect("kill the child");
    child.wait().expect("reap the child");
}

/// The next process: the same store file, the start-up recovery, a drain through `smtp`.
async fn next_process(path: &Path, smtp: &str) -> SqliteStore<SystemClock> {
    let store = SqliteStore::open(path, SystemClock).unwrap();
    store.recover_interrupted_ops().await.unwrap();
    drain_outbox(
        &plaintext(smtp),
        &store,
        &account(),
        WorkerId::new("next"),
        TTL,
    )
    .await
    .unwrap();
    store
}

async fn state(store: &SqliteStore<SystemClock>) -> PendingOpState {
    let rows = store.list_pending_ops(account()).await.unwrap();
    rows.first()
        .map_or(PendingOpState::Succeeded, |row| row.state)
}

/// A point of the conversation the scripted server reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    Accepted,
    Ehlo,
    Rcpt,
    Body,
    Dot,
    Replied,
}

/// A plaintext SMTP server that holds its first connection at `kill_at` until released, and
/// serves every other connection whole. A message counts when its `.` arrives.
struct Scripted {
    addr: String,
    delivered: Arc<AtomicUsize>,
    reached: mpsc::UnboundedReceiver<Point>,
    release: Arc<Notify>,
}

impl Scripted {
    async fn start(kill_at: Option<Point>, greeting_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let delivered = Arc::new(AtomicUsize::new(0));
        let (tx, reached) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let armed = Arc::new(AtomicBool::new(kill_at.is_some()));
        let (count, waiting) = (Arc::clone(&delivered), Arc::clone(&release));
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let hold = armed
                    .swap(false, Ordering::SeqCst)
                    .then_some(kill_at)
                    .flatten();
                let (tx, count, waiting) = (tx.clone(), Arc::clone(&count), Arc::clone(&waiting));
                tokio::spawn(async move {
                    let _ = serve(tcp, hold, greeting_delay, &tx, &count, &waiting).await;
                });
            }
        });
        Self {
            addr,
            delivered,
            reached,
            release,
        }
    }

    async fn reaches(&mut self, point: Point) {
        let wait = async {
            while let Some(reached) = self.reached.recv().await {
                if reached == point {
                    return;
                }
            }
        };
        tokio::time::timeout(Duration::from_mins(1), wait)
            .await
            .unwrap_or_else(|_| panic!("the child never reached {point:?}"));
    }

    fn delivered(&self) -> usize {
        self.delivered.load(Ordering::SeqCst)
    }
}

async fn serve(
    tcp: TcpStream,
    hold: Option<Point>,
    greeting_delay: Duration,
    tx: &mpsc::UnboundedSender<Point>,
    delivered: &AtomicUsize,
    release: &Notify,
) -> std::io::Result<()> {
    let at = |point: Point| async move {
        let _ = tx.send(point);
        if hold == Some(point) {
            release.notified().await;
        }
    };
    let mut io = BufReader::new(tcp);
    at(Point::Accepted).await;
    tokio::time::sleep(greeting_delay).await;
    io.write_all(b"220 mail ESMTP ready\r\n").await?;
    loop {
        let mut line = String::new();
        if io.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            at(Point::Ehlo).await;
            b"250 mail\r\n"
        } else if upper.starts_with("RCPT") {
            at(Point::Rcpt).await;
            b"250 ok\r\n"
        } else if upper.starts_with("MAIL") {
            b"250 ok\r\n"
        } else if upper.starts_with("DATA") {
            io.write_all(b"354 go ahead\r\n").await?;
            let mut first = true;
            loop {
                let mut data = String::new();
                if io.read_line(&mut data).await? == 0 {
                    return Ok(());
                }
                if data == ".\r\n" {
                    break;
                }
                if first {
                    first = false;
                    at(Point::Body).await;
                }
            }
            delivered.fetch_add(1, Ordering::SeqCst);
            at(Point::Dot).await;
            io.write_all(b"250 queued\r\n").await?;
            io.flush().await?;
            at(Point::Replied).await;
            continue;
        } else if upper.starts_with("QUIT") {
            let _ = io.write_all(b"221 bye\r\n").await;
            return Ok(());
        } else {
            continue;
        };
        io.write_all(reply).await?;
    }
}

fn spec(store: &Path, smtp: &str, id: &str) -> Spec {
    Spec {
        store: store.to_string_lossy().into_owned(),
        smtp: smtp.to_owned(),
        imap: None,
        from: "alice@test.local".to_owned(),
        message_id: format!("{id}-{}@test.local", std::process::id()),
        big: false,
        drain: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_killed_anywhere_in_a_send_delivers_it_once_or_asks() {
    for point in [
        Point::Accepted,
        Point::Ehlo,
        Point::Rcpt,
        Point::Body,
        Point::Dot,
        Point::Replied,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let mut server = Scripted::start(Some(point), Duration::ZERO).await;
        let mut job = spec(&path, &server.addr, &format!("kill-{point:?}"));
        job.big = point == Point::Body;

        let mut child = spawn(&job);
        server.reaches(point).await;
        kill(&mut child);
        server.release.notify_waiters();

        let store = next_process(&path, &server.addr).await;
        assert_eq!(
            server.delivered(),
            1,
            "{point:?}: the recipient has it once"
        );
        let settled = state(&store).await;
        match point {
            // Nothing had reached the server's end of the message: an ordinary retry.
            Point::Accepted | Point::Ehlo | Point::Rcpt | Point::Body => {
                assert_eq!(settled, PendingOpState::Succeeded, "{point:?}");
            }
            // It had: possibly delivered, so never resent.
            Point::Dot => assert_eq!(settled, PendingOpState::NeedsConfirmation),
            // Killed as the acceptance arrived: either it heard it, or it asks.
            Point::Replied => assert!(
                matches!(
                    settled,
                    PendingOpState::Succeeded | PendingOpState::NeedsConfirmation
                ),
                "{settled:?}"
            ),
        }
    }
}

/// Two processes drain one store file at once, as an app and its background task can: the
/// message goes out once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_draining_one_store_send_a_message_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    // A slow greeting, so both drains are in their claims while the first attempt runs.
    let mut server = Scripted::start(Some(Point::Accepted), Duration::from_millis(300)).await;
    let job = spec(&path, &server.addr, "raced");
    let mut first = spawn(&job);
    server.reaches(Point::Accepted).await;
    kill(&mut first);
    server.release.notify_waiters();
    let store = SqliteStore::open(&path, SystemClock).unwrap();
    assert_eq!(store.recover_interrupted_ops().await.unwrap(), 1);
    drop(store);

    let drain = Spec { drain: true, ..job };
    let mut racers = [spawn(&drain), spawn(&drain)];
    for racer in &mut racers {
        let status = racer.wait().unwrap();
        assert!(status.success(), "{status}");
    }
    assert_eq!(server.delivered(), 1);
    let store = SqliteStore::open(&path, SystemClock).unwrap();
    assert_eq!(state(&store).await, PendingOpState::Succeeded);
}

#[path = "process_kill_stalwart_tests.rs"]
mod stalwart_tests;
