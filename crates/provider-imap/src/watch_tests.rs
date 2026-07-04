//! Watcher tests over a real in-memory `tokio::io::duplex` — a bidirectional pipe
//! whose reads genuinely block until the "server" half writes, so the keep-alive
//! timeout and the stay-idling-across-events behavior are exercised for real (unlike
//! the scripted Cursor mock, which EOFs when its script runs out). The keep-alive
//! tests use `start_paused` so the 28-minute virtual timer fires instantly with no
//! real waiting.

use std::time::Duration;

use engine_core::error::FailureClass;
use engine_core::ids::MailboxId;
use engine_provider::{Watch, WatchEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};

use super::{DEFAULT_IDLE_KEEPALIVE, ImapWatcher};
use crate::transport::Connection;

/// Reads one CRLF-terminated line the client wrote.
async fn read_line(server: &mut BufReader<DuplexStream>) -> String {
    let mut line = String::new();
    server.read_line(&mut line).await.expect("server read");
    line
}

/// Writes raw server bytes to the client and flushes.
async fn write(server: &mut BufReader<DuplexStream>, bytes: &str) {
    server
        .write_all(bytes.as_bytes())
        .await
        .expect("server write");
    server.flush().await.expect("server flush");
}

/// Greets, accepts login, advertises IDLE + QRESYNC, ENABLEs, and EXAMINEs — leaving
/// the client ready to issue its first `a5 IDLE`. The caller keeps driving the server
/// half for the IDLE phase.
async fn serve_handshake(server: &mut BufReader<DuplexStream>) {
    write(server, "* OK ready\r\n").await;
    assert!(read_line(server).await.contains("LOGIN"));
    write(server, "a1 OK logged in\r\n").await;
    assert!(read_line(server).await.contains("CAPABILITY"));
    write(
        server,
        "* CAPABILITY IMAP4rev2 IDLE CONDSTORE QRESYNC\r\na2 OK done\r\n",
    )
    .await;
    assert!(read_line(server).await.contains("ENABLE"));
    write(server, "* ENABLED QRESYNC\r\na3 OK enabled\r\n").await;
    assert!(read_line(server).await.contains("EXAMINE"));
    write(
        server,
        "* 3 EXISTS\r\n* OK [UIDVALIDITY 1] ok\r\na4 OK [READ-ONLY] done\r\n",
    )
    .await;
}

/// Drives the client side of [`serve_handshake`]: opens, logs in, negotiates, and
/// starts a watcher bound to INBOX.
async fn start_watcher(client: DuplexStream, keepalive: Duration) -> ImapWatcher<DuplexStream> {
    let mut conn = Connection::open(client).await.unwrap();
    conn.login("u", "p").await.unwrap();
    conn.negotiate_qresync().await.unwrap();
    ImapWatcher::start(conn, MailboxId::try_from("INBOX").unwrap(), keepalive)
        .await
        .unwrap()
}

#[tokio::test]
async fn reports_a_change_and_keeps_idling_for_the_next() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE")); // a5 IDLE
        write(&mut server, "+ idling\r\n").await;
        // Two changes on the SAME IDLE: the watcher must stay idling (no DONE between
        // them), so a change arriving while the host syncs the first is not lost.
        write(&mut server, "* 4 EXISTS\r\n").await;
        write(&mut server, "* 5 EXISTS\r\n").await;
        server // keep the write half alive (in the JoinHandle) so the pipe stays open
    });

    let mut watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    assert!(format!("{watcher:?}").contains("ImapWatcher"));
    assert_eq!(watcher.next_event().await.unwrap(), WatchEvent::Changed);
    assert_eq!(watcher.next_event().await.unwrap(), WatchEvent::Changed);

    drop(watcher);
    server.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn re_idles_and_reports_keepalive_on_a_quiet_interval() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE")); // a5 IDLE
        write(&mut server, "+ idling\r\n").await;
        // Stay quiet; the watcher's keep-alive elapses (virtual 28 min) and it DONEs.
        assert_eq!(read_line(&mut server).await.trim(), "DONE");
        write(&mut server, "a5 OK IDLE terminated\r\n").await;
        server
    });

    let mut watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    assert_eq!(watcher.next_event().await.unwrap(), WatchEvent::KeepAlive);

    drop(watcher);
    server.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_change_at_the_keepalive_boundary_is_reported_as_changed() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE"));
        write(&mut server, "+ idling\r\n").await;
        // Quiet until the keep-alive fires; then answer the DONE with a change *before*
        // the tagged completion — it must surface as Changed, not swallowed.
        assert_eq!(read_line(&mut server).await.trim(), "DONE");
        write(&mut server, "* 9 EXISTS\r\na5 OK IDLE terminated\r\n").await;
        server
    });

    let mut watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    assert_eq!(watcher.next_event().await.unwrap(), WatchEvent::Changed);

    drop(watcher);
    server.await.unwrap();
}

#[tokio::test]
async fn start_fails_without_idle_capability() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        write(&mut server, "* OK ready\r\n").await;
        assert!(read_line(&mut server).await.contains("LOGIN"));
        write(&mut server, "a1 OK\r\n").await;
        assert!(read_line(&mut server).await.contains("CAPABILITY"));
        // QRESYNC advertised, but NOT IDLE → the watcher cannot push.
        write(
            &mut server,
            "* CAPABILITY IMAP4rev2 CONDSTORE QRESYNC\r\na2 OK\r\n",
        )
        .await;
        assert!(read_line(&mut server).await.contains("ENABLE"));
        write(&mut server, "* ENABLED QRESYNC\r\na3 OK\r\n").await;
        server
    });

    let mut conn = Connection::open(client).await.unwrap();
    conn.login("u", "p").await.unwrap();
    conn.negotiate_qresync().await.unwrap();
    let err = ImapWatcher::start(
        conn,
        MailboxId::try_from("INBOX").unwrap(),
        DEFAULT_IDLE_KEEPALIVE,
    )
    .await
    .unwrap_err();
    // A non-IDLE server is InvalidState — the host falls back to polling, never retries.
    assert_eq!(err.class(), FailureClass::InvalidState);

    server.await.unwrap();
}

#[tokio::test]
async fn surfaces_a_dropped_connection_as_retryable() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE"));
        write(&mut server, "+ idling\r\n").await;
        drop(server); // the socket dies mid-idle → the client read hits EOF
    });

    let mut watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    let err = watcher.next_event().await.unwrap_err();
    // A transport drop is retryable: the host reconnects (and re-syncs first).
    assert_eq!(err.class(), FailureClass::Retryable);

    server.await.unwrap();
}

#[tokio::test]
async fn surfaces_a_server_bye_as_retryable() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE"));
        write(&mut server, "+ idling\r\n").await;
        write(&mut server, "* BYE server going down\r\n").await;
        server
    });

    let mut watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    assert_eq!(
        watcher.next_event().await.unwrap_err().class(),
        FailureClass::Retryable
    );

    drop(watcher);
    server.await.unwrap();
}

#[tokio::test]
async fn drives_through_dyn_watch() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE"));
        write(&mut server, "+ idling\r\n").await;
        write(&mut server, "* 7 EXISTS\r\n").await;
        server
    });

    // A host holds the session behind dynamic dispatch (adapter chosen at runtime).
    let mut watch: Box<dyn Watch> = Box::new(start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await);
    assert_eq!(watch.next().await.unwrap(), WatchEvent::Changed);

    drop(watch);
    server.await.unwrap();
}

#[tokio::test]
async fn stop_ends_idle_with_a_done() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        assert!(read_line(&mut server).await.contains("IDLE"));
        write(&mut server, "+ idling\r\n").await;
        write(&mut server, "* 4 EXISTS\r\n").await;
        // The watcher stays idling after the change, so stop() must end IDLE cleanly.
        assert_eq!(read_line(&mut server).await.trim(), "DONE");
        write(&mut server, "a5 OK terminated\r\n").await;
        server
    });

    let mut watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    assert_eq!(watcher.next_event().await.unwrap(), WatchEvent::Changed);
    watcher.stop().await.unwrap();

    server.await.unwrap();
}

#[tokio::test]
async fn stop_is_a_noop_when_not_idling() {
    let (client, server) = duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        serve_handshake(&mut server).await;
        server // no IDLE is ever issued
    });

    // A freshly started watcher has not idled yet, so stop() sends no DONE.
    let watcher = start_watcher(client, DEFAULT_IDLE_KEEPALIVE).await;
    watcher.stop().await.unwrap();

    server.await.unwrap();
}
