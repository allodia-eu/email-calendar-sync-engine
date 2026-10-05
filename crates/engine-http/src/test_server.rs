//! TEST BUILDS ONLY: HTTP servers that stop answering, for meeting a silent server through
//! a real transport.
//!
//! Behind the `test-server` feature, which only the adapters' dev-dependencies turn on, so
//! each adapter's suite drives its own funnel against the same servers rather than keeping a
//! copy of them. Plain HTTP on the loopback address, in the caller's runtime, so a test on
//! tokio's paused clock passes a one-minute bound in no wall time.
//!
//! ⚠️ **On the paused clock, bytes in flight lose to every timer.** tokio moves the paused
//! clock to the next timer whenever every task is waiting, and a task waiting for a socket
//! counts as waiting even while the kernel is delivering bytes to it. How long loopback takes
//! to report them is the operating system's business: Linux usually reports them before the
//! runtime next looks, macOS often does not, and a reply then meets a one-minute bound it never
//! spent. So nothing here lets the clock move while an exchange the test expects is under way:
//!
//! - [`hold_clock`] keeps the clock still (a blocking task inhibits tokio's advance for as long as
//!   it runs), and [`held_until`] holds it until an event a test names has happened, such as
//!   [`SilentServer::has_received`];
//! - [`trickling`] holds it from the start until the client has the head, and again from each piece
//!   it writes until the client has read it, so only its own pauses between pieces pass on the
//!   clock.
//!
//! The client's side of that is the funnel counting each step a reply takes on its thread (a
//! head arriving, a read of the body), which only builds with this feature count.

use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};

/// How long a hold may last in real time before it lets go on its own: far longer than any
/// exchange here takes, and only there so a broken client fails the test rather than hanging
/// it with the clock stopped.
const HOLD_AT_MOST: Duration = Duration::from_secs(30);

/// The paused clock, held where it is until this is dropped.
#[derive(Debug)]
#[must_use = "the clock moves again as soon as this is dropped"]
pub struct HeldClock {
    _release: std::sync::mpsc::Sender<()>,
}

/// Holds tokio's paused clock still: a blocking task inhibits its advance for as long as it
/// runs, and this one runs until the [`HeldClock`] is dropped.
pub fn hold_clock() -> HeldClock {
    let (release, held) = std::sync::mpsc::channel::<()>();
    // Detached: it ends when `release` is dropped, or after `HOLD_AT_MOST`.
    drop(tokio::task::spawn_blocking(move || {
        let _ = held.recv_timeout(HOLD_AT_MOST);
    }));
    HeldClock { _release: release }
}

/// Runs `work` with the paused clock held until `event` has happened, and moving from then on:
/// the clock passes only the silence `work` meets after it.
pub async fn held_until<T>(event: impl Future<Output = ()>, work: impl Future<Output = T>) -> T {
    let hold = hold_clock();
    let release = async move {
        event.await;
        drop(hold);
    };
    let (done, ()) = tokio::join!(work, release);
    done
}

/// How many steps replies have taken on this thread (`deadline::PROGRESS`).
fn progress() -> u64 {
    crate::deadline::PROGRESS.with(core::cell::Cell::get)
}

/// Waits, with the clock held by the caller, for a reply on this thread to take a step past
/// `from`.
async fn client_moves_past(from: u64) {
    let started = std::time::Instant::now();
    while progress() <= from && started.elapsed() < HOLD_AT_MOST {
        tokio::task::yield_now().await;
    }
}

/// A server that answers a script of replies, one per request, and then reads every later
/// request in full and answers nothing, holding its connection open.
#[derive(Debug)]
pub struct SilentServer {
    url: String,
    received: watch::Receiver<usize>,
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
        let (count, received) = watch::channel(0);
        let count = Arc::new(count);
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
        *self.received.borrow()
    }

    /// Resolves once the server has read `count` requests in full: the event to hold the paused
    /// clock until ([`held_until`]), so a silence starts only once the request is with the
    /// server.
    pub async fn has_received(&self, count: usize) {
        let mut received = self.received.clone();
        let _ = received.wait_for(|seen| *seen >= count).await;
    }
}

/// Starts a server that answers its first request with `head` (a status line and headers,
/// ending in a blank line) and then sends `piece` `pieces` times, one every `every`,
/// holding the connection open in silence once they are spent. Returns its origin, as
/// [`SilentServer::url`] does.
///
/// The paused clock is held from now until the client has the head, and from each piece until
/// the client has read it, so the only time that passes is the `every` between pieces and the
/// silence after the last.
///
/// # Panics
///
/// If no loopback port can be bound.
pub async fn trickling(head: String, piece: String, pieces: usize, every: Duration) -> String {
    trickling_late(head, piece, pieces, every, Duration::ZERO).await
}

/// [`trickling`], with every write reaching the socket `latency` of real time after the server
/// makes it, from a thread of its own: loopback as slow to report bytes as some systems are.
/// What it shows is that a test holding the clock does not depend on how quickly they come.
///
/// # Panics
///
/// If no loopback port can be bound.
pub async fn trickling_late(
    head: String,
    piece: String,
    pieces: usize,
    every: Duration,
    latency: Duration,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let until_the_head = hold_clock();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let _ = read_request(&mut stream, &mut Vec::new()).await;
        let mut out = Writer::new(stream, latency);
        let read = out.deliver(head.into_bytes()).await;
        drop(until_the_head);
        if read.is_err() {
            return;
        }
        for _ in 0..pieces {
            tokio::time::sleep(every).await;
            let _held = hold_clock();
            if out.deliver(piece.clone().into_bytes()).await.is_err() {
                return;
            }
        }
        std::future::pending::<()>().await;
    });
    url
}

/// Where a trickling server's bytes go: straight to the socket, or through a thread that
/// writes them late.
enum Writer {
    Now(TcpStream),
    Late(std::net::TcpStream, Duration),
}

impl Writer {
    fn new(stream: TcpStream, latency: Duration) -> Self {
        if latency.is_zero() {
            return Self::Now(stream);
        }
        let stream = stream.into_std().expect("a std socket");
        stream.set_nonblocking(false).expect("a blocking socket");
        Self::Late(stream, latency)
    }

    /// Writes `bytes`, then waits for the client to take a step with them; the caller holds
    /// the clock.
    async fn deliver(&mut self, bytes: Vec<u8>) -> std::io::Result<()> {
        let from = progress();
        match self {
            Self::Now(stream) => stream.write_all(&bytes).await?,
            Self::Late(stream, latency) => {
                let (mut writer, latency) = (stream.try_clone()?, *latency);
                std::thread::spawn(move || {
                    std::thread::sleep(latency);
                    let _ = std::io::Write::write_all(&mut writer, &bytes);
                });
            }
        }
        client_moves_past(from).await;
        Ok(())
    }
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
async fn answer(
    mut stream: TcpStream,
    replies: Arc<Vec<String>>,
    received: Arc<watch::Sender<usize>>,
) {
    let mut buffer = Vec::new();
    while let Some(length) = read_request(&mut stream, &mut buffer).await {
        buffer.drain(..length);
        let mut index = 0;
        received.send_modify(|seen| {
            index = *seen;
            *seen += 1;
        });
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
