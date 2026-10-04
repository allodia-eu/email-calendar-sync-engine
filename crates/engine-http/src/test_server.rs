//! TEST BUILDS ONLY: HTTP servers that stop answering, for meeting a silent server through
//! a real transport.
//!
//! Behind the `test-server` feature, which only the adapters' dev-dependencies turn on, so
//! each adapter's suite drives its own funnel against the same servers rather than keeping a
//! copy of them. Plain HTTP on the loopback address, in the caller's runtime, so a test on
//! tokio's paused clock passes a one-minute bound in no wall time.
//!
//! ⚠️ On the paused clock, a task waiting for a socket moves the clock on to the next timer
//! even when the socket already has bytes for it. A request a server *answers* therefore
//! spends paused time it never spent, and enough of those waits reach a bound. Answer what a
//! test needs answered before pausing, and pause for the silence (`tokio::time::pause`).

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// A server that answers a script of replies, one per request, and then reads every later
/// request in full and answers nothing, holding its connection open.
#[derive(Debug)]
pub struct SilentServer {
    url: String,
    received: Arc<AtomicUsize>,
}

impl SilentServer {
    /// Starts a server that answers `replies` in order (each a whole HTTP/1.1 response, see
    /// [`reply`]) and is silent from then on.
    ///
    /// # Panics
    ///
    /// If no loopback port can be bound.
    pub async fn start(replies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let received = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&received);
        let replies = Arc::new(replies);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(answer(stream, Arc::clone(&replies), Arc::clone(&count)));
            }
        });
        Self { url, received }
    }

    /// The server's origin, `http://127.0.0.1:<port>`, with no trailing slash.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// How many requests the server has read in full, answered or not.
    #[must_use]
    pub fn received(&self) -> usize {
        self.received.load(Ordering::SeqCst)
    }
}

/// Starts a server that answers its first request with `head` (a status line and headers,
/// ending in a blank line) and then sends `piece` `pieces` times, one every `every`,
/// holding the connection open in silence once they are spent. Returns its origin, as
/// [`SilentServer::url`] does.
///
/// # Panics
///
/// If no loopback port can be bound.
pub async fn trickling(head: String, piece: String, pieces: usize, every: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let _ = read_request(&mut stream, &mut Vec::new()).await;
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for _ in 0..pieces {
            tokio::time::sleep(every).await;
            if stream.write_all(piece.as_bytes()).await.is_err() {
                return;
            }
        }
        std::future::pending::<()>().await;
    });
    url
}

/// A whole HTTP/1.1 response with `status` (such as `"200 OK"`), `content_type` and `body`.
#[must_use]
pub fn reply(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Reads requests off one connection, answering each with the next reply in the script while
/// there is one and then holding the connection open in silence.
async fn answer(mut stream: TcpStream, replies: Arc<Vec<String>>, received: Arc<AtomicUsize>) {
    let mut buffer = Vec::new();
    while let Some(length) = read_request(&mut stream, &mut buffer).await {
        buffer.drain(..length);
        let index = received.fetch_add(1, Ordering::SeqCst);
        let Some(reply) = replies.get(index) else {
            // Silent, and still connected: the shape a paused server behind a forwarder takes.
            std::future::pending::<()>().await;
            return;
        };
        if stream.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Reads one whole request (its head and a `Content-Length` body) into `buffer`, returning
/// its length, or `None` once the client has closed the connection.
async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<usize> {
    loop {
        if let Some(head) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let end = head + 4 + content_length(&buffer[..head]);
            if buffer.len() >= end {
                return Some(end);
            }
        }
        let mut chunk = [0_u8; 8 * 1024];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
}

/// The `Content-Length` a request head declares, or zero.
fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}
