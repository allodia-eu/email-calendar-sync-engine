//! What [`send_retrying`] does with a throttled reply, against a scripted server.

use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::{RetryConfig, RetryPolicy, ThrottleEvent, send_retrying};

/// The client every provider here builds, so these send through the stack that ships.
fn client() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("client")
}

/// One reply the scripted server will make: a status line and any extra header lines.
struct Reply(&'static str, &'static str);

/// Serves `script` in order, repeating its last entry once exhausted, and counts what it was
/// asked for. Every reply closes its connection, so each attempt is visible as its own accept.
fn scripted(script: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
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
            let Reply(status, headers) = &script[index.min(script.len() - 1)];
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/"), served)
}

/// Every event the run reported, in order.
type Log = Arc<Mutex<Vec<(u16, u32, Duration, bool, bool)>>>;

fn recording() -> (RetryConfig, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let observer = move |e: &ThrottleEvent<'_>| {
        sink.lock()
            .unwrap()
            .push((e.status, e.attempt, e.delay, e.server_asked, e.gave_up));
    };
    (
        RetryConfig::default()
            .labelled("test")
            .with_observer(Arc::new(observer)),
        log,
    )
}

#[tokio::test(start_paused = true)]
async fn a_reply_that_is_not_a_throttle_is_returned_after_one_send() {
    let (url, served) = scripted(vec![Reply("200 OK", "")]);
    let (retry, log) = recording();
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(served.load(Ordering::SeqCst), 1);
    assert!(log.lock().unwrap().is_empty(), "nothing to report");
}

#[tokio::test(start_paused = true)]
async fn a_throttle_that_clears_is_absorbed() {
    let (url, served) = scripted(vec![
        Reply("429 Too Many Requests", ""),
        Reply("200 OK", ""),
    ]);
    let (retry, log) = recording();
    let started = tokio::time::Instant::now();
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200, "the caller sees success");
    assert_eq!(served.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "it waited before sending again",
    );
    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1);
    let (status, attempt, _, asked, gave_up) = events[0];
    assert_eq!((status, attempt), (429, 0));
    assert!(!asked, "no Retry-After was sent");
    assert!(!gave_up);
}

#[tokio::test(start_paused = true)]
async fn a_throttle_that_never_clears_is_handed_back_as_the_rate_limit_it_is() {
    let (url, served) = scripted(vec![Reply("429 Too Many Requests", "")]);
    let (retry, log) = recording();
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(
        response.status().as_u16(),
        429,
        "waiting is absorbed, the outcome is not",
    );
    assert_eq!(served.load(Ordering::SeqCst), 5, "the default attempt cap");
    let events = log.lock().unwrap();
    assert_eq!(events.len(), 5, "four waits and the give-up");
    assert!(
        events[..4].iter().all(|e| !e.4),
        "only the last event gives up",
    );
    let (_, _, spent, _, gave_up) = events[4];
    assert!(gave_up);
    assert!(
        spent >= Duration::from_millis(3750),
        "the give-up reports the total spent: {spent:?}",
    );
}

#[tokio::test(start_paused = true)]
async fn the_servers_own_retry_after_decides_the_wait() {
    let (url, _) = scripted(vec![
        Reply("429 Too Many Requests", "Retry-After: 12\r\n"),
        Reply("200 OK", ""),
    ]);
    let (retry, log) = recording();
    let started = tokio::time::Instant::now();
    send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert!(
        started.elapsed() >= Duration::from_secs(12),
        "backoff would have guessed 250ms and been refused again",
    );
    let (_, _, delay, asked, _) = log.lock().unwrap()[0];
    assert!(asked);
    assert!(delay >= Duration::from_secs(12));
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_given_as_a_date_is_honoured_too() {
    // The scripted server needs a real instant, because the date form is resolved against the
    // system clock rather than the runtime's — which is the whole reason it is guarded.
    let when = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(20));
    let header: &'static str = Box::leak(format!("Retry-After: {when}\r\n").into_boxed_str());
    let (url, served) = scripted(vec![
        Reply("429 Too Many Requests", header),
        Reply("200 OK", ""),
    ]);
    let (retry, log) = recording();
    let started = tokio::time::Instant::now();
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(served.load(Ordering::SeqCst), 2);
    let waited = started.elapsed();
    // The band absorbs a second the header cannot carry: `fmt_http_date` truncates to whole
    // seconds, so the instant it names is up to a second nearer than the 20s asked for, and
    // the delay is then measured from a `now` read after the round trip. Both losses are on
    // the same side, and 18s is still three orders of magnitude from the quarter-second the
    // backoff schedule would have chosen, which is what this distinguishes.
    assert!(
        waited >= Duration::from_secs(18) && waited <= Duration::from_secs(22),
        "waited {waited:?}, want the ~20s the date named",
    );
    assert!(
        log.lock().unwrap()[0].3,
        "reported as the server's own number"
    );
}

#[tokio::test(start_paused = true)]
async fn a_date_already_in_the_past_falls_back_to_the_backoff_schedule() {
    // A device clock running fast turns a live quota window into a past instant. The wait
    // must not collapse to nothing: that would spend every attempt inside one second.
    let when = httpdate::fmt_http_date(std::time::SystemTime::now() - Duration::from_mins(10));
    let header: &'static str = Box::leak(format!("Retry-After: {when}\r\n").into_boxed_str());
    let (url, served) = scripted(vec![
        Reply("429 Too Many Requests", header),
        Reply("200 OK", ""),
    ]);
    let (retry, log) = recording();
    let started = tokio::time::Instant::now();
    send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(served.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "it still backed off",
    );
    assert!(
        !log.lock().unwrap()[0].3,
        "and did not credit the server with a number it could not use",
    );
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_past_the_budget_hands_the_work_to_the_next_pass() {
    let (url, served) = scripted(vec![Reply("429 Too Many Requests", "Retry-After: 900\r\n")]);
    let (retry, log) = recording();
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(served.load(Ordering::SeqCst), 1, "it did not park the task");
    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].4, "reported as a give-up, not a silent stall");
    assert!(events[0].3, "and as the server's own number");
}

#[tokio::test(start_paused = true)]
async fn a_503_is_waited_out_for_a_get_and_never_for_a_post() {
    let (url, served) = scripted(vec![
        Reply("503 Service Unavailable", ""),
        Reply("200 OK", ""),
    ]);
    let (retry, _) = recording();
    send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(served.load(Ordering::SeqCst), 2);

    let (post_url, post_served) = scripted(vec![Reply("503 Service Unavailable", "")]);
    let response = send_retrying(client().post(&post_url).body("x"), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(
        post_served.load(Ordering::SeqCst),
        1,
        "a replayed POST on a 503 is a message sent twice",
    );
}

#[tokio::test(start_paused = true)]
async fn the_none_policy_reports_the_throttle_without_waiting_it_out() {
    let (url, served) = scripted(vec![Reply("429 Too Many Requests", "")]);
    let (retry, log) = recording();
    let retry = retry.with_policy(RetryPolicy::none());
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(served.load(Ordering::SeqCst), 1);
    assert_eq!(log.lock().unwrap().len(), 1, "still reported");
    assert!(log.lock().unwrap()[0].4);
}

#[tokio::test(start_paused = true)]
async fn a_host_that_wires_no_observer_still_gets_the_backoff() {
    let (url, served) = scripted(vec![
        Reply("429 Too Many Requests", ""),
        Reply("200 OK", ""),
    ]);
    let response = send_retrying(client().get(&url), &RetryConfig::default().labelled("test"))
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(served.load(Ordering::SeqCst), 2);
}
