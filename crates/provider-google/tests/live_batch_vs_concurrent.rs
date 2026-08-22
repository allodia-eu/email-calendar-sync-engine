//! Gated head-to-head: Gmail's batch endpoint against a bounded concurrent fan-out, over
//! the **same** pooled `reqwest` client the adapter uses.
//!
//! The question is not academic. `messages.list` returns bare ids, so a snapshot is one
//! `messages.get` per message and the only choice is how to overlap them. Batching *looks*
//! like the obvious answer — one request instead of twenty — and only a live probe settles
//! it:
//!
//! - Whether Google's front end really multiplexes twenty concurrent `GET`s onto one HTTP/2
//!   connection, or serializes them behind a per-connection limit.
//! - Whether a batch of n is billed and paced as one request or as n.
//! - What the multipart envelope costs in bytes and in parse work.
//!
//! What it has measured so far, at width 20 over fibre:
//!
//! - Every response is HTTP/2 on one pooled connection, so the fan-out really is multiplexed rather
//!   than serialized behind a per-connection limit.
//! - The two shapes are **indistinguishable on latency**. Repeated runs put each median between
//!   roughly 220ms and 300ms and hand the win to either side depending on the run — both cost one
//!   round trip per round, and that is the whole story. Do not read a single run's gap as signal;
//!   that mistake has been made here more than once.
//! - The batch envelope costs about **a quarter more bytes**, every run without exception.
//! - Both throttle as the width grows, which is the direct evidence that a batch of n counts as n
//!   requests. Batch tolerates a somewhat wider window before it does.
//!
//! So the case for concurrency is bytes, simplicity, and the ability to yield a message as it
//! lands rather than after a whole envelope parses — not speed. Re-run this before changing
//! `MAX_CONCURRENT_GETS` or reaching for batch again.
//!
//! `GOOGLE_BENCH_WINDOW` sets the width, `GOOGLE_BENCH_ROUNDS` how long the rate is held.
//!
//! Measured over `reqwest` rather than curl on purpose: curl's `--parallel-immediate` opens
//! fresh connections instead of multiplexing, which measures the opposite of what the engine
//! does. Everything here shares one `Client`, so the connection pool behaves as it does in
//! the adapter.
//!
//! Read-only: it lists ids and fetches them. Skips unless `GOOGLE_ACCESS_TOKEN` is set.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_batch_vs_concurrent -- --nocapture
//! ```

use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};

/// Messages per round — the adapter's in-flight window, and Google's own advice is to keep
/// a batch at or under 50. Override with `GOOGLE_BENCH_WINDOW` to sweep for the knee.
fn window() -> usize {
    std::env::var("GOOGLE_BENCH_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}
/// Rounds per shape. Enough to see a median rather than one sample's weather; raise it with
/// `GOOGLE_BENCH_ROUNDS` to hold the rate long enough to provoke a cumulative throttle.
/// Cold rounds sampled per shape. Odd, so the median is a real sample.
const COLD_SAMPLES: usize = 5;

fn rounds() -> usize {
    std::env::var("GOOGLE_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15)
}

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const BATCH: &str = "https://gmail.googleapis.com/batch/gmail/v1";
const BOUNDARY: &str = "batch_probe";

/// Counts convert through `u32` because clippy's pedantic set denies a bare `usize as f64`;
/// nothing measured here approaches that bound.
fn count_as_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// The envelope headers the adapter asks for (`normalize::METADATA_HEADERS`).
fn metadata_query() -> String {
    let mut q = String::from("format=metadata");
    for header in [
        "From",
        "To",
        "Cc",
        "Bcc",
        "Subject",
        "Date",
        "Message-ID",
        "In-Reply-To",
        "References",
    ] {
        q.push_str("&metadataHeaders=");
        q.push_str(header);
    }
    q
}

/// One `reqwest` client, built the way the adapter builds its own (ALPN h2, pooled).
fn pooled_client() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("client")
}

/// A summary of one shape's rounds.
struct Timings {
    label: &'static str,
    samples: Vec<Duration>,
    bytes: usize,
    throttled: usize,
}

impl Timings {
    fn median(&self) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    fn report(&self, window: usize) {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2].as_secs_f64();
        println!(
            "  {:<28} median {:>7.0}ms   min {:>6.0}ms   max {:>6.0}ms   \
             {:>5.0} ms/msg   {:>6.1} msg/s   {:>7} B/round   {} throttled",
            self.label,
            median * 1000.0,
            sorted[0].as_secs_f64() * 1000.0,
            sorted[sorted.len() - 1].as_secs_f64() * 1000.0,
            median * 1000.0 / count_as_f64(window),
            count_as_f64(window) / median,
            self.bytes / self.samples.len().max(1),
            self.throttled,
        );
    }
}

/// Lists up to `want` message ids, repeating the mailbox's own if it is smaller.
async fn ids(client: &reqwest::Client, token: &str, want: usize) -> Vec<String> {
    let doc: serde_json::Value = client
        .get(format!(
            "{API}/messages?maxResults=100&includeSpamTrash=true"
        ))
        .bearer_auth(token)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let found: Vec<String> = doc
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    assert!(!found.is_empty(), "the live account should hold some mail");
    found.iter().cycle().take(want).cloned().collect()
}

/// One round of `WINDOW` individual gets, all in flight together on the pooled client.
async fn concurrent_round(
    client: &reqwest::Client,
    token: &str,
    ids: &[String],
    query: &str,
) -> (usize, usize, bool) {
    let results: Vec<(usize, u16, bool)> = stream::iter(ids.to_vec())
        .map(|id| async move {
            let resp = client
                .get(format!("{API}/messages/{id}?{query}"))
                .bearer_auth(token)
                .send()
                .await
                .expect("get");
            let status = resp.status().as_u16();
            let http2 = resp.version() == reqwest::Version::HTTP_2;
            let len = resp.bytes().await.expect("body").len();
            (len, status, http2)
        })
        .buffered(window())
        .collect()
        .await;
    let bytes = results.iter().map(|(l, ..)| l).sum();
    let throttled = results.iter().filter(|(_, s, _)| *s == 429).count();
    let ok = results.iter().filter(|(_, s, _)| *s == 200).count();
    assert!(
        ok + throttled == results.len(),
        "{} of {} gets answered neither 200 nor 429 — a timing sample over failed requests \
         is not a measurement",
        results.len() - ok - throttled,
        results.len(),
    );
    let all_h2 = results.iter().all(|(_, _, h)| *h);
    (bytes, throttled, all_h2)
}

/// One round of the same messages as a single multipart batch `POST`.
async fn batch_round(
    client: &reqwest::Client,
    token: &str,
    ids: &[String],
    query: &str,
) -> (usize, usize, bool) {
    let mut body = String::new();
    for (i, id) in ids.iter().enumerate() {
        let _ = write!(
            body,
            "--{BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: <i{i}>\r\n\r\n\
             GET /gmail/v1/users/me/messages/{id}?{query}\r\n\r\n"
        );
    }
    let _ = write!(body, "--{BOUNDARY}--\r\n");

    let resp = client
        .post(BATCH)
        .bearer_auth(token)
        .header(
            "Content-Type",
            format!("multipart/mixed; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .expect("batch");
    let http2 = resp.version() == reqwest::Version::HTTP_2;
    let status = resp.status();
    let text = resp.text().await.expect("batch body");
    // Each sub-response carries its own status line; a batch answers 200 overall while
    // individual members fail, which is the trap that makes batching look clean. Counting
    // only 429 would also let a *fast failure* — an expired token 401ing every member in
    // 90ms — post the best time in the run and be read as batch winning.
    let throttled = text.matches("HTTP/1.1 429").count();
    let ok = text.matches("HTTP/1.1 200").count();
    assert!(
        status.is_success() && ok + throttled == ids.len(),
        "batch answered {status} with {ok} ok + {throttled} throttled of {} — a timing \
         sample over failed sub-responses is not a measurement",
        ids.len(),
    );
    (text.len(), throttled, http2)
}

#[tokio::test]
async fn live_batch_versus_concurrent() {
    let Some(token) = token() else {
        eprintln!("skipping live_batch_versus_concurrent: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let query = metadata_query();
    let client = pooled_client();
    let width = window();
    let ids = ids(&client, &token, width).await;

    println!(
        "\n{width} messages per round, {} rounds, one pooled HTTP/2 client\n",
        rounds()
    );

    // Cold rounds on a fresh client each time, so the TLS handshake is inside the
    // measurement — the cost a first sync after launch really pays. Sampled rather than
    // taken once: a single cold round is dominated by whatever the network did that second,
    // and reading one as signal is how this probe would start lying.
    let mut cold_concurrent_samples = Vec::new();
    let mut cold_batch_samples = Vec::new();
    let mut cold_h2 = true;
    for _ in 0..COLD_SAMPLES {
        let cold = pooled_client();
        let started = Instant::now();
        let (_, _, h2) = concurrent_round(&cold, &token, &ids, &query).await;
        cold_concurrent_samples.push(started.elapsed());
        cold_h2 &= h2;

        let cold = pooled_client();
        let started = Instant::now();
        batch_round(&cold, &token, &ids, &query).await;
        cold_batch_samples.push(started.elapsed());
    }
    cold_concurrent_samples.sort_unstable();
    cold_batch_samples.sort_unstable();
    let cold_concurrent = cold_concurrent_samples[COLD_SAMPLES / 2];
    let cold_batch = cold_batch_samples[COLD_SAMPLES / 2];

    let mut concurrent = Timings {
        label: "concurrent (20 in flight)",
        samples: Vec::new(),
        bytes: 0,
        throttled: 0,
    };
    let mut batch = Timings {
        label: "batch (1 request, 20 subs)",
        samples: Vec::new(),
        bytes: 0,
        throttled: 0,
    };
    let mut all_h2 = cold_h2;

    let total_rounds = rounds();
    for _ in 0..total_rounds {
        let started = Instant::now();
        let (bytes, throttled, h2) = concurrent_round(&client, &token, &ids, &query).await;
        concurrent.samples.push(started.elapsed());
        concurrent.bytes += bytes;
        concurrent.throttled += throttled;
        all_h2 &= h2;

        let started = Instant::now();
        let (bytes, throttled, h2) = batch_round(&client, &token, &ids, &query).await;
        batch.samples.push(started.elapsed());
        batch.bytes += bytes;
        batch.throttled += throttled;
        all_h2 &= h2;
    }

    println!("warm (connection already pooled):");
    concurrent.report(width);
    batch.report(width);
    println!("\ncold (TLS handshake inside the measurement, median of {COLD_SAMPLES}):");
    println!(
        "  {:<28} median {:>5.0}ms   min {:>5.0}ms   max {:>5.0}ms",
        "concurrent (20 in flight)",
        cold_concurrent.as_secs_f64() * 1000.0,
        cold_concurrent_samples[0].as_secs_f64() * 1000.0,
        cold_concurrent_samples[COLD_SAMPLES - 1].as_secs_f64() * 1000.0,
    );
    println!(
        "  {:<28} median {:>5.0}ms   min {:>5.0}ms   max {:>5.0}ms",
        "batch (1 request, 20 subs)",
        cold_batch.as_secs_f64() * 1000.0,
        cold_batch_samples[0].as_secs_f64() * 1000.0,
        cold_batch_samples[COLD_SAMPLES - 1].as_secs_f64() * 1000.0,
    );
    println!(
        "\n  every response HTTP/2: {all_h2}\n  \
         handshake overhead — concurrent {:.0}ms, batch {:.0}ms\n",
        (cold_concurrent.as_secs_f64() - concurrent.median().as_secs_f64()) * 1000.0,
        (cold_batch.as_secs_f64() - batch.median().as_secs_f64()) * 1000.0,
    );

    // The measurement is a print, not a threshold — someone else's service over whatever
    // link the developer has is not an assertion. This one property is: a batch that answers
    // 200 while throttling its members is the failure mode that makes batching look free.
    assert_eq!(
        concurrent.throttled, 0,
        "a bounded concurrent fan-out must not be throttled"
    );
}
