//! The scripted HTTP server the two `send_retrying` suites share.
//!
//! Its own module because there are two of them — `send_tests` for the status rule,
//! `classify_send_tests` for the bodies an adapter classifies — and a second copy of a
//! server is a second thing to keep in step with the funnel it is testing.

use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::{RetryConfig, ThrottleEvent};

/// The client every provider here builds, so these send through the stack that ships.
pub(crate) fn client() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("client")
}

/// One reply the scripted server will make: a status line, any extra header lines, and a
/// body. The body is usually empty — it matters only for the replies a
/// [`ThrottleClassifier`] has to read, which is the whole point of those.
pub(crate) struct Reply(
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) &'static str,
);

/// Serves `script` in order, repeating its last entry once exhausted, and counts what it was
/// asked for. Every reply closes its connection, so each attempt is visible as its own accept.
pub(crate) fn scripted(script: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let served = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf);
            let index = count.fetch_add(1, Ordering::SeqCst);
            let Reply(status, headers, body) = &script[index.min(script.len() - 1)];
            let length = body.len();
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {length}\r\n\
                     Connection: close\r\n\r\n{body}"
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/"), served)
}

/// Every event the run reported, in order.
pub(crate) type Log = Arc<Mutex<Vec<(u16, u32, Duration, Option<Duration>, bool)>>>;

pub(crate) fn recording() -> (RetryConfig, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let observer = move |e: &ThrottleEvent<'_>| {
        sink.lock()
            .unwrap()
            .push((e.status, e.attempt, e.delay, e.stated, e.gave_up));
    };
    (
        RetryConfig::default()
            .labelled("test")
            .with_observer(Arc::new(observer)),
        log,
    )
}
