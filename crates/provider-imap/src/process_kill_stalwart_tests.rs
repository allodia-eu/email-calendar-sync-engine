//! The child process killed mid-send against the Stalwart harness: its SMTP connection goes
//! through a byte proxy here, which holds it at the point and kills the child there.
//!
//! The scratch account sends to itself, never the seeded account whose counts other suites
//! assert, under a `Message-ID` unique to the run; every copy is deleted afterwards. Skips
//! with no harness configured.

use engine_core::{
    mail::{MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::MailEdit;
use stalwart_harness::Harness;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

/// Where the proxy holds the child's connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Accepted, never connected onward: the child is still waiting for a greeting.
    Connecting,
    /// The whole message, its end included, went to the server, and the server's answer is
    /// held back.
    AfterHandOver,
}

/// A TCP proxy to `upstream` that holds its first connection as `hold` says, waking `reached`
/// when it does, and passes every later connection straight through, counting into `ended`
/// every message whose end it forwarded to the server.
///
/// That count is what reached the server, which the recipient's mailbox cannot be trusted to
/// show: a server may suppress a second delivery of one `Message-ID`.
async fn proxy(
    upstream: String,
    hold: Hold,
    reached: Arc<Notify>,
    ended: Arc<AtomicUsize>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let mut armed = true;
        while let Ok((client, _)) = listener.accept().await {
            let holding = core::mem::take(&mut armed).then_some(hold);
            let (upstream, reached) = (upstream.clone(), Arc::clone(&reached));
            let ended = Arc::clone(&ended);
            tokio::spawn(async move {
                if holding == Some(Hold::Connecting) {
                    reached.notify_one();
                    let _keep = client;
                    return std::future::pending::<()>().await;
                }
                let server = TcpStream::connect(&upstream).await.unwrap();
                pump(client, server, holding.is_some(), &reached, &ended).await;
            });
        }
    });
    addr
}

/// Copies both ways. When `hold` is set, the first bytes the server sends after the client
/// has sent the end of a message (`CRLF . CRLF`) are kept back, and `reached` is woken.
async fn pump(
    client: TcpStream,
    server: TcpStream,
    hold: bool,
    reached: &Notify,
    count: &AtomicUsize,
) {
    let (mut client_rx, mut client_tx) = client.into_split();
    let (mut server_rx, mut server_tx) = server.into_split();
    let ended = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&ended);
    let outbound = async move {
        let mut tail = Vec::new();
        let mut buf = vec![0_u8; 64 * 1024];
        while let Ok(n) = client_rx.read(&mut buf).await {
            if n == 0 || server_tx.write_all(&buf[..n]).await.is_err() {
                return;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.windows(5).any(|w| w == b"\r\n.\r\n") {
                seen.store(true, Ordering::SeqCst);
                count.fetch_add(1, Ordering::SeqCst);
                tail.clear();
            }
            let keep = tail.len().saturating_sub(4);
            tail.drain(..keep);
        }
    };
    let inbound = async move {
        let mut buf = vec![0_u8; 64 * 1024];
        while let Ok(n) = server_rx.read(&mut buf).await {
            if n == 0 {
                return;
            }
            if hold && ended.load(Ordering::SeqCst) {
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

/// The scratch account's IMAP side, to count and delete what arrived.
async fn mailbox(
    harness: &Harness,
    folder: &str,
) -> ImapProvider<tokio_rustls::client::TlsStream<TcpStream>> {
    let [bob, _] = &harness.scratch;
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = crate::ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        crate::Credentials::password(bob.address.as_str(), bob.password.as_str()),
    );
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    crate::ImapAccount::connect(&config, connector)
        .await
        .expect("connect to the harness")
        .provider(MailboxId::try_from(folder).unwrap())
}

/// The scratch account's folder named for `role`.
async fn folder(harness: &Harness, role: MailboxRole, fallback: &str) -> String {
    let SyncUpdate::Snapshot { objects, .. } = mailbox(harness, "INBOX")
        .await
        .sync_mailboxes(&account(), None)
        .await
        .expect("list folders")
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.role.as_ref() == Some(&role))
        .map_or_else(|| fallback.to_owned(), |m| m.name)
}

/// The messages in `folder` whose `Message-ID` `owned` accepts.
async fn copies(harness: &Harness, folder: &str, owned: impl Fn(&str) -> bool) -> Vec<Message> {
    let SyncUpdate::Snapshot { objects, .. } = mailbox(harness, folder)
        .await
        .sync_email(&account(), None)
        .await
        .expect("read the folder")
        .update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .filter(|m| m.envelope.message_id.iter().any(|id| owned(id.as_str())))
        .collect()
}

/// How many copies reached the recipient, once delivery has had time to finish. The harness
/// files mail from its own domain arriving unauthenticated on port 25 as junk, so the inbox
/// and the junk folder together are what was delivered.
async fn delivered(harness: &Harness, message_id: &str) -> usize {
    let junk = folder(harness, MailboxRole::Junk, "Junk Mail").await;
    let count = || async {
        let mut total = 0;
        for place in ["INBOX", junk.as_str()] {
            total += copies(harness, place, |id| id == message_id).await.len();
        }
        total
    };
    for _ in 0..60 {
        if count().await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // A second copy, if one were coming, has the same queue to go through.
    tokio::time::sleep(Duration::from_secs(3)).await;
    count().await
}

/// Deletes every copy this suite has left anywhere it delivers or files: its `Message-ID`s
/// all start with [`PREFIX`], so nothing another suite owns is touched.
async fn clean_up(harness: &Harness) {
    let sent = folder(harness, MailboxRole::Sent, "Sent").await;
    let junk = folder(harness, MailboxRole::Junk, "Junk Mail").await;
    for place in ["INBOX", junk.as_str(), sent.as_str()] {
        let provider = mailbox(harness, place).await;
        for copy in copies(harness, place, |id| id.starts_with(PREFIX)).await {
            let _ = provider
                .edit_mail(&account(), &MailEdit::delete(copy.id.key().clone()))
                .await;
        }
    }
}

/// The start of every `Message-ID` this suite sends.
const PREFIX: &str = "harness-kill-";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_killed_mid_send_on_the_harness_delivers_once() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipped: no Stalwart harness configured");
        return;
    };
    let [bob, _] = &harness.scratch;
    for (hold, settled) in [
        (Hold::Connecting, PendingOpState::Succeeded),
        (Hold::AfterHandOver, PendingOpState::NeedsConfirmation),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (reached, ended) = (Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0)));
        let through = proxy(
            harness.smtp_addr.clone(),
            hold,
            Arc::clone(&reached),
            Arc::clone(&ended),
        )
        .await;
        let job = Spec {
            imap: Some((
                harness.imap_addr.clone(),
                bob.address.clone(),
                bob.password.clone(),
            )),
            from: bob.address.clone(),
            ..spec(&path, &through, &format!("{PREFIX}{hold:?}"))
        };

        let mut child = spawn(&job);
        tokio::time::timeout(Duration::from_mins(1), reached.notified())
            .await
            .unwrap_or_else(|_| panic!("the child never reached {hold:?}"));
        kill(&mut child);

        // The next process, through the same proxy, which now passes everything.
        let store = SqliteStore::open(&path, SystemClock).unwrap();
        store.recover_interrupted_ops().await.unwrap();
        let (user, password) = (bob.address.as_str(), bob.password.as_str());
        let direct = stalwart(&harness.imap_addr, &through, user, password).await;
        drain_outbox(&direct, &store, &account(), WorkerId::new("next"), TTL)
            .await
            .unwrap();

        let count = delivered(&harness, &job.message_id).await;
        clean_up(&harness).await;
        assert_eq!(
            ended.load(Ordering::SeqCst),
            1,
            "{hold:?}: the server was handed the message once"
        );
        assert_eq!(count, 1, "{hold:?}: the recipient has it once");
        assert_eq!(state(&store).await, settled, "{hold:?}");
    }
}
