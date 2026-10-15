//! Where an HTTP submission records its hand-over: after the connection is up, before the
//! last piece of its body, and never twice.
//!
//! The server here counts the bytes it has read as they arrive, and the log looks at that
//! count when the hand-over is recorded, after giving any byte already sent time to land. A
//! gate that let the end of the body out first would show the whole request there.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use engine_provider::{HandOver, HandOverLog};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use crate::{Exchange, RetryConfig, send_retrying};

/// A server that reads every byte it is sent, counting them, and answers each whole request
/// with the next reply in its script, then holds the connection open.
struct CountingServer {
    url: String,
    accepted: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
}

impl CountingServer {
    async fn start(replies: Vec<&'static str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/send", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let (seen, read) = (Arc::clone(&accepted), Arc::clone(&bytes));
        tokio::spawn(async move {
            let replies = Arc::new(Mutex::new(replies.into_iter()));
            while let Ok((mut stream, _)) = listener.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
                let (read, replies) = (Arc::clone(&read), Arc::clone(&replies));
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = vec![0_u8; 64 * 1024];
                    loop {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        read.fetch_add(n, Ordering::SeqCst);
                        buffer.extend_from_slice(&chunk[..n]);
                        while let Some(end) = whole_request(&buffer) {
                            buffer.drain(..end);
                            let next = replies.lock().unwrap().next();
                            let Some(reply) = next else { return };
                            if stream.write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Self {
            url,
            accepted,
            bytes,
        }
    }
}

/// The length of the first whole request in `buffer`, by its `Content-Length`.
fn whole_request(buffer: &[u8]) -> Option<usize> {
    let head = buffer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let length = String::from_utf8_lossy(&buffer[..head])
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let end = head + 4 + length;
    (buffer.len() >= end).then_some(end)
}

/// A log that, when the hand-over is recorded, notes what the server had by then.
struct Watching {
    accepted: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
    seen: Mutex<Vec<(usize, usize)>>,
    refuse: bool,
}

impl Watching {
    fn on(server: &CountingServer, refuse: bool) -> Self {
        Self {
            accepted: Arc::clone(&server.accepted),
            bytes: Arc::clone(&server.bytes),
            seen: Mutex::new(Vec::new()),
            refuse,
        }
    }
}

#[async_trait::async_trait]
impl HandOverLog for Watching {
    async fn record_hand_over(&self) -> Result<(), String> {
        // Long enough for anything already written to reach the server.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let snapshot = (
            self.accepted.load(Ordering::SeqCst),
            self.bytes.load(Ordering::SeqCst),
        );
        self.seen.lock().unwrap().push(snapshot);
        if self.refuse {
            return Err("disk full".to_owned());
        }
        Ok(())
    }
}

fn client() -> reqwest::Client {
    crate::client(&engine_tls::TlsClientConfig::bundled())
        .build()
        .unwrap()
}

const ACCEPTED: &str = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n";
const THROTTLED: &str =
    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";

#[tokio::test]
async fn the_hand_over_is_recorded_on_an_open_connection_before_the_end_of_the_body() {
    for size in [10, 200 * 1024] {
        let server = CountingServer::start(vec![ACCEPTED]).await;
        let log = Watching::on(&server, false);
        let hand_over = HandOver::new(&log);
        let body = vec![b'x'; size];

        let sent = send_retrying(
            client().post(&server.url).body(body),
            &RetryConfig::default(),
            Exchange::Submission(&hand_over),
        )
        .await
        .unwrap();
        assert_eq!(sent.status().as_u16(), 202);

        let seen = log.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "recorded once");
        let (accepted, bytes) = seen[0];
        assert_eq!(
            accepted, 1,
            "{size}: the connection was up before the record"
        );
        let total = server.bytes.load(Ordering::SeqCst);
        let last_piece = match size % crate::deadline::PIECE {
            0 => crate::deadline::PIECE,
            rest => rest,
        };
        assert!(
            bytes < total && total - bytes >= last_piece,
            "{size}: the server had {bytes} of {total} bytes at the record, the end still to come"
        );
    }
}

#[tokio::test]
async fn a_hand_over_that_cannot_be_recorded_withholds_the_end_of_the_body() {
    let server = CountingServer::start(vec![ACCEPTED]).await;
    let log = Watching::on(&server, true);
    let hand_over = HandOver::new(&log);
    let size = 200 * 1024;

    let err = send_retrying(
        client().post(&server.url).body(vec![b'x'; size]),
        &RetryConfig::default(),
        Exchange::Submission(&hand_over),
    )
    .await
    .expect_err("the request was abandoned");

    assert!(!err.may_have_been_received(), "{err}");
    assert!(err.to_string().contains("disk full"), "{err}");
    assert!(!hand_over.is_committed());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        server.bytes.load(Ordering::SeqCst) < size,
        "the server never had the whole body"
    );
}

#[tokio::test]
async fn a_throttled_submission_sent_again_records_its_hand_over_once() {
    let server = CountingServer::start(vec![THROTTLED, ACCEPTED]).await;
    let log = Watching::on(&server, false);
    let hand_over = HandOver::new(&log);

    let sent = send_retrying(
        client().post(&server.url).body("a message"),
        &RetryConfig::default(),
        Exchange::Submission(&hand_over),
    )
    .await
    .unwrap();

    assert_eq!(sent.status().as_u16(), 202);
    assert_eq!(log.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_submission_with_no_body_records_its_hand_over_before_it_is_sent() {
    let server = CountingServer::start(vec![ACCEPTED]).await;
    let log = Watching::on(&server, false);
    let hand_over = HandOver::new(&log);

    send_retrying(
        client()
            .post(&server.url)
            .header(reqwest::header::CONTENT_LENGTH, 0),
        &RetryConfig::default(),
        Exchange::Submission(&hand_over),
    )
    .await
    .unwrap();

    assert_eq!(log.seen.lock().unwrap().clone(), vec![(0, 0)]);
}
