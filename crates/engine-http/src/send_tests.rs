//! What [`send_retrying`] does with a reply the **status** classifies, against a scripted
//! server. The bodies an adapter classifies are the sibling suite, `classify_send_tests`.

use std::{
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::{
    RetryConfig, RetryPolicy, send_retrying,
    send_test_support::{Reply, client, recording, scripted},
};

#[tokio::test(start_paused = true)]
async fn a_reply_that_is_not_a_throttle_is_returned_after_one_send() {
    let (url, served) = scripted(vec![Reply("200 OK", "", "")]);
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
        Reply("429 Too Many Requests", "", ""),
        Reply("200 OK", "", ""),
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
    let (url, served) = scripted(vec![Reply("429 Too Many Requests", "", "")]);
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
        Reply("429 Too Many Requests", "Retry-After: 12\r\n", ""),
        Reply("200 OK", "", ""),
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
        Reply("429 Too Many Requests", header, ""),
        Reply("200 OK", "", ""),
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
        Reply("429 Too Many Requests", header, ""),
        Reply("200 OK", "", ""),
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
    let (url, served) = scripted(vec![Reply(
        "429 Too Many Requests",
        "Retry-After: 900\r\n",
        "",
    )]);
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
        Reply("503 Service Unavailable", "", ""),
        Reply("200 OK", "", ""),
    ]);
    let (retry, _) = recording();
    send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(served.load(Ordering::SeqCst), 2);

    let (post_url, post_served) = scripted(vec![Reply("503 Service Unavailable", "", "")]);
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
    let (url, served) = scripted(vec![Reply("429 Too Many Requests", "", "")]);
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
        Reply("429 Too Many Requests", "", ""),
        Reply("200 OK", "", ""),
    ]);
    let response = send_retrying(client().get(&url), &RetryConfig::default().labelled("test"))
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(served.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_account_ceiling_bounds_what_is_in_flight_across_providers() {
    // The measured shape: a mailbox that is clean at four concurrent requests and refuses
    // at five. The two configs stand for two of an account's providers — a folder-bound
    // mail provider and its calendar — which is exactly the pair that has to be counted
    // together, and separately labelled because they report as themselves.
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&live);
    let high = Arc::clone(&peak);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (counter, high) = (Arc::clone(&counter), Arc::clone(&high));
            std::thread::spawn(move || {
                let mut buf = [0_u8; 4096];
                let _ = stream.read(&mut buf);
                let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                counter.fetch_sub(1, Ordering::SeqCst);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    let url = format!("http://{addr}/");

    let gate = crate::RequestGate::new();
    let mail = RetryConfig::default().labelled("graph").gated(gate.clone());
    let calendar = RetryConfig::default()
        .labelled("graph-calendar")
        .gated(gate.clone());
    // What the adapter states as it connects, once, for the whole account.
    gate.narrow_to(4);

    let client = client();
    let mut handles = Vec::new();
    for (index, retry) in (0..16).map(|i| (i, if i % 2 == 0 { &mail } else { &calendar })) {
        let (client, url, retry) = (client.clone(), url.clone(), retry.clone());
        handles.push(tokio::spawn(async move {
            let _ = index;
            send_retrying(client.get(&url), &retry).await.expect("sent");
        }));
    }
    for handle in handles {
        handle.await.expect("task");
    }
    assert_eq!(
        peak.load(Ordering::SeqCst),
        4,
        "two providers of one account were counted apart",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_is_held_across_the_backoff_and_not_handed_on_mid_throttle() {
    // A request waiting out a `Retry-After` is still a request in progress. Releasing its
    // lane for the wait would let the next one take it and be refused in turn, which is how
    // one throttle becomes several — the thing the jitter above already exists to avoid.
    let (url, _served) = scripted(vec![
        Reply("429 Too Many Requests", "Retry-After: 1\r\n", ""),
        Reply("200 OK", "", ""),
    ]);
    let gate = crate::RequestGate::new();
    gate.narrow_to(1);
    let retry = RetryConfig::default().labelled("test").gated(gate.clone());
    let throttled = {
        let (client, url, retry) = (client(), url.clone(), retry.clone());
        tokio::spawn(async move { send_retrying(client.get(&url), &retry).await.expect("sent") })
    };
    // Long enough that the first request is certainly inside its one-second backoff.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let taken = tokio::time::timeout(Duration::from_millis(300), gate.acquire()).await;
    assert!(
        taken.is_err(),
        "the retrying request gave up its lane while the server was asking for less",
    );
    assert_eq!(throttled.await.expect("task").status().as_u16(), 200);
}
