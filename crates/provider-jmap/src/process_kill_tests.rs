//! A JMAP send through the outbox in a **child process** killed (`SIGKILL` on Unix) mid-send,
//! against the Stalwart harness, after which this process opens the same store file, runs the
//! start-up recovery, a mail sync and a drain, and counts what the recipient received.
//!
//! The child is this test binary run again with [`CHILD`] set. Its requests go through a byte
//! proxy here, which holds them at the point and kills the child there:
//!
//! - before the request that submits (the attempt is still reading the account's context), where
//!   the send is retried;
//! - after the submitting request went to the server whole, with its answer held back, where the
//!   send awaits confirmation and the server's copy in Sent confirms it.
//!
//! The scratch account sends to itself under a `Message-ID` unique to the run, and every copy
//! is deleted afterwards. Skips with no harness configured.

use core::time::Duration;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, Message},
    sync::SyncUpdate,
    time::UtcDateTime,
};
use engine_provider::{Draft, MailEdit, Provider};
use engine_store::{Clock, PendingOpState, Store, StoreRead, WorkerId};
use engine_sync::{IgnoreCommits, StreamTuning, drain_outbox, submit_mail, sync_mail};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

use crate::{Credentials, JmapConfig, JmapProvider};

const CHILD: &str = "ENGINE_TEST_DURABLE_JMAP_CHILD";
const ENTRY: &str = "process_kill_tests::a_child_process_sends_when_asked";
const TTL: Duration = Duration::from_mins(5);
/// The start of every `Message-ID` this suite sends.
const PREFIX: &str = "jmap-harness-kill-";

/// The wall clock, which the child and this process share.
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
    AccountId::try_from("acct-jmap-process").unwrap()
}

async fn connect(base: &str, user: &str, password: &str) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(base, Credentials::basic(user, password)))
        .await
        .expect("connect")
}

/// Not a test of its own: the body of a child process, and a no-op in an ordinary run. The
/// spec is `store`, `base`, `user`, `password`, `message_id`, one per line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_process_sends_when_asked() {
    let Ok(spec) = std::env::var(CHILD) else {
        return;
    };
    let [store, base, user, password, message_id]: [&str; 5] =
        spec.lines().collect::<Vec<_>>().try_into().unwrap();
    let store = SqliteStore::open(store, SystemClock).unwrap();
    let provider = connect(base, user, password).await;
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(user),
        vec![EmailAddress::new(user)],
        "Durable send",
        "Body text",
    );
    let worker = WorkerId::new(format!("process-{}", std::process::id()));
    let _ = submit_mail(&provider, &store, &account(), worker, TTL, &draft).await;
}

/// Where the proxy holds the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// At the first request of the attempt (the context read, before anything that can
    /// deliver): the request is never forwarded.
    BeforeSubmitting,
    /// The submitting request reached the server whole; its answer is held back.
    AfterHandOver,
}

/// A TCP proxy to `upstream` holding the child's traffic as `hold` says, waking `reached`
/// when it does. Every connection is watched until the hold happens, then passed through.
async fn proxy(upstream: String, hold: Hold, reached: Arc<Notify>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let armed = Arc::new(AtomicBool::new(true));
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = TcpStream::connect(&upstream).await.unwrap();
            let (armed, reached) = (Arc::clone(&armed), Arc::clone(&reached));
            tokio::spawn(pump(client, server, hold, armed, reached));
        }
    });
    format!("http://{addr}")
}

async fn pump(
    client: TcpStream,
    server: TcpStream,
    hold: Hold,
    armed: Arc<AtomicBool>,
    reached: Arc<Notify>,
) {
    let (mut client_rx, mut client_tx) = client.into_split();
    let (mut server_rx, mut server_tx) = server.into_split();
    let submitted = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&submitted);
    let (out_armed, out_reached) = (Arc::clone(&armed), Arc::clone(&reached));
    let outbound = async move {
        let mut buf = vec![0_u8; 256 * 1024];
        let mut window = Vec::new();
        while let Ok(n) = client_rx.read(&mut buf).await {
            if n == 0 {
                return;
            }
            window.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&window);
            if out_armed.load(Ordering::SeqCst)
                && hold == Hold::BeforeSubmitting
                && text.contains("Identity/get")
                && out_armed.swap(false, Ordering::SeqCst)
            {
                out_reached.notify_one();
                return std::future::pending::<()>().await;
            }
            if text.contains("EmailSubmission/set") {
                seen.store(true, Ordering::SeqCst);
            }
            if server_tx.write_all(&buf[..n]).await.is_err() {
                return;
            }
            let keep = window.len().saturating_sub(64);
            window.drain(..keep);
        }
    };
    let inbound = async move {
        let mut buf = vec![0_u8; 256 * 1024];
        while let Ok(n) = server_rx.read(&mut buf).await {
            if n == 0 {
                return;
            }
            if hold == Hold::AfterHandOver
                && submitted.load(Ordering::SeqCst)
                && armed.swap(false, Ordering::SeqCst)
            {
                reached.notify_one();
                return std::future::pending::<()>().await;
            }
            if client_tx.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    };
    tokio::join!(outbound, inbound);
}

fn spawn(spec: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([ENTRY, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, spec)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the child")
}

/// The messages anywhere in the account whose `Message-ID` `owned` accepts, with the mailboxes
/// they are filed in.
async fn copies(provider: &JmapProvider, owned: impl Fn(&str) -> bool) -> Vec<Message> {
    let SyncUpdate::Snapshot { objects, .. } =
        provider.sync_email(&account(), None).await.unwrap().update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .filter(|m| m.envelope.message_id.iter().any(|id| owned(id.as_str())))
        .collect()
}

/// How many copies the recipient got (filed anywhere but Sent) and how many the sender's Sent
/// holds, once delivery has had time to finish.
///
/// Sent is the count that shows a second submission: each one files its own copy there, while
/// a server may suppress a second delivery of one `Message-ID` to the recipient.
async fn delivered(provider: &JmapProvider, message_id: &str) -> (usize, usize) {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .unwrap()
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    let sent = objects
        .into_iter()
        .find(|m| m.role == Some(MailboxRole::Sent))
        .expect("a Sent mailbox")
        .id;
    let count = || async {
        let all = copies(provider, |id| id == message_id).await;
        let filed = all.iter().filter(|m| m.mailboxes.contains(&sent)).count();
        (all.len() - filed, filed)
    };
    for _ in 0..40 {
        if count().await.0 > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    count().await
}

async fn state(store: &SqliteStore<SystemClock>) -> PendingOpState {
    let rows = store.list_pending_ops(account()).await.unwrap();
    rows.first()
        .map_or(PendingOpState::Succeeded, |row| row.state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_killed_mid_send_on_the_harness_delivers_once() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipped: no Stalwart harness configured");
        return;
    };
    let [bob, _] = &harness.scratch;
    let direct = format!("http://{}", harness.http_addr);
    let provider = connect(&direct, &bob.address, &bob.password).await;
    for hold in [Hold::BeforeSubmitting, Hold::AfterHandOver] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let reached = Arc::new(Notify::new());
        let through = proxy(harness.http_addr.clone(), hold, Arc::clone(&reached)).await;
        let message_id = format!("{PREFIX}{hold:?}-{}@test.local", std::process::id());
        let spec = [
            path.to_string_lossy().as_ref(),
            through.as_str(),
            bob.address.as_str(),
            bob.password.as_str(),
            message_id.as_str(),
        ]
        .join("\n");

        let mut child = spawn(&spec);
        tokio::time::timeout(Duration::from_mins(1), reached.notified())
            .await
            .unwrap_or_else(|_| panic!("the child never reached {hold:?}"));
        child.kill().expect("kill the child");
        child.wait().expect("reap the child");

        let store = next_process(&path, &provider).await;
        let (received, filed) = delivered(&provider, &message_id).await;
        for copy in copies(&provider, |id| id.starts_with(PREFIX)).await {
            let _ = provider
                .edit_mail(&account(), &MailEdit::delete(copy.id.key().clone()))
                .await;
        }
        assert_eq!(filed, 1, "{hold:?}: submitted once, so filed in Sent once");
        assert_eq!(received, 1, "{hold:?}: the recipient has it once");
        // Before the submitting request it was simply sent again; after it, the server's
        // copy in Sent confirmed it, and nothing sent it twice.
        assert_eq!(state(&store).await, PendingOpState::Succeeded, "{hold:?}");
    }
}

/// The next process: the start-up recovery, a mail sync (which confirms a send whose copy the
/// server filed in Sent), and a drain.
async fn next_process(path: &Path, provider: &JmapProvider) -> SqliteStore<SystemClock> {
    let store = SqliteStore::open(path, SystemClock).unwrap();
    store.recover_interrupted_ops().await.unwrap();
    // Until the copy is filed and synced, a send that may have gone stays unconfirmed.
    for _ in 0..20 {
        sync_mail(
            core::slice::from_ref(provider),
            &store,
            &account(),
            WorkerId::new("next"),
            TTL,
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await;
        if state(&store).await != PendingOpState::NeedsConfirmation {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    drain_outbox(provider, &store, &account(), WorkerId::new("next"), TTL)
        .await
        .unwrap();
    store
}
