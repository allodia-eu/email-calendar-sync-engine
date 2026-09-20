//! Gated live measurement of what a DAV server actually refuses — width, or rate.
//!
//! `http-throttling.md` recorded "unknown" for the DAV adapters and left the account gate
//! un-narrowed on that basis. "Unknown" is an honest placeholder and a poor resting place: it
//! reads equally as "there is no ceiling" and as "nobody looked", and the next agent to meet a
//! DAV `429` has no way to tell which. This looks.
//!
//! # The question a gate can answer
//!
//! `engine_http::RequestGate` bounds **requests in flight**. So the only finding that would
//! justify a DAV adapter narrowing one is a ceiling on *width*. A limit on request *rate* is a
//! different quantity that a width bound does not control: halving the width of a drain that
//! runs twice as long delivers the same requests per second.
//!
//! Hence two arms, and neither is optional:
//!
//! - **A — fixed rate, varying width.** If refusals stay flat, width is not the lever.
//! - **B — fixed width, varying rate.** This is what stops arm A being vacuous. A server that
//!   simply never refuses anything produces arm A's zeros for free, and "no ceiling found" would
//!   then mean "the probe could not provoke a refusal" (`AGENTS.md`: an absence must be proved
//!   against the request that asks for it; #102 is what skipping that costs).
//!
//! # What it found
//!
//! Measured on this harness, `PROPFIND` + `calendar-query` REPORT, six-second blocks. One
//! observed run — the refusal *shares* move between runs (150/s gave 11.5% once and 4.8%
//! another time), so read the shape, not the numbers:
//!
//! | | Stalwart | SabreDAV |
//! |---|---|---|
//! | A: 25/s at widths 1, 4, 16, 64 | 0% refused throughout | 0% refused throughout |
//! | B: width 8 at 10, 50, 150, 400/s | 0%, 0%, **a few %**, **tens of %** | 0% throughout |
//!
//! So **neither implementation bounds concurrency**, Stalwart bounds *rate* (it trips
//! somewhere between 50/s and 150/s and says so with a `429`), and SabreDAV refused nothing at
//! all up to 400/s. Two independent implementations, because one server's behaviour is a fact
//! about that server while the adapter's ceiling is a claim about CalDAV.
//!
//! Which is also why arm A asserts and arm B only reports: "clean at every width" is a
//! property worth failing on, while "refused 40.6% at 400/s" is a weather report, and a
//! threshold against it would be a flake generator.
//!
//! ⚠️ **It takes about seven minutes**, nearly all of it the cooldowns. They are the
//! measurement: without them a block inherits the previous block's drained limiter, which is
//! exactly the mistake that produced the wrong Gmail figure this file's sibling corrects.
//!
//! That is why no DAV adapter calls `RequestGate::narrow_to`, and this test is what a future
//! change to that decision has to get past.
//!
//! Skips unless the harness addresses are set, like every live suite here:
//!
//! ```sh
//! scripts/ci/stalwart-live.sh up
//! docker compose -f docker/sabredav/docker-compose.yml up -d --wait
//! STALWART_HTTP_ADDR=127.0.0.1:18080 SABREDAV_HTTP_ADDR=127.0.0.1:18081 \
//!   cargo test -p provider-caldav --all-features --test live_concurrency -- --nocapture
//! ```

use core::time::Duration;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Long enough for a tripped rate limiter to refill between blocks, so a block measures its
/// own offered load rather than the previous block's leftovers. The first DAV sweep skipped
/// this and produced a "width" result that was pure carry-over.
const COOLDOWN: Duration = Duration::from_secs(20);

/// How long each block offers load for.
const BLOCK: Duration = Duration::from_secs(6);

/// A server to measure, and the two URLs the adapter's own shapes go to.
struct Target {
    name: &'static str,
    home: String,
    collection: String,
    user: String,
    pass: String,
}

fn stalwart() -> Option<Target> {
    let addr = std::env::var("STALWART_HTTP_ADDR")
        .ok()
        .filter(|a| !a.is_empty())?;
    Some(Target {
        name: "stalwart",
        home: format!("http://{addr}/dav/cal/alice%40test.local/"),
        collection: format!("http://{addr}/dav/cal/alice%40test.local/default/"),
        user: "alice@test.local".to_owned(),
        pass: std::env::var("STALWART_PASSWORD").unwrap_or_else(|_| "harness-alice-pw".to_owned()),
    })
}

fn sabredav() -> Option<Target> {
    let addr = std::env::var("SABREDAV_HTTP_ADDR")
        .ok()
        .filter(|a| !a.is_empty())?;
    Some(Target {
        name: "sabredav",
        home: format!("http://{addr}/calendars/alice@test.local/"),
        collection: format!("http://{addr}/calendars/alice@test.local/default/"),
        user: std::env::var("SABREDAV_USER").unwrap_or_else(|_| "alice@test.local".to_owned()),
        pass: std::env::var("SABREDAV_PASS").unwrap_or_else(|_| "sabredav-alice-pw".to_owned()),
    })
}

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#;

const REPORT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:prop><d:getetag/></d:prop>
<c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"/></c:comp-filter></c:filter>
</c:calendar-query>"#;

/// What one block observed.
#[derive(Default)]
struct Counts {
    ok: AtomicUsize,
    refused: AtomicUsize,
    other: AtomicUsize,
}

impl Counts {
    fn total(&self) -> usize {
        self.ok.load(Ordering::SeqCst)
            + self.refused.load(Ordering::SeqCst)
            + self.other.load(Ordering::SeqCst)
    }

    /// The refused share as a percentage. Counts here are hundreds, so the `u32` narrowing
    /// is free — and it is what keeps the lossy-cast lint honest rather than silenced.
    fn refused_percent(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        let refused = u32::try_from(self.refused.load(Ordering::SeqCst)).unwrap_or(u32::MAX);
        let total = u32::try_from(total).unwrap_or(u32::MAX);
        f64::from(refused) * 100.0 / f64::from(total)
    }
}

/// Offers `rate` requests per second for [`BLOCK`], never more than `width` in flight.
///
/// Alternates the two shapes `provider-caldav` sends, so a refusal here is one the adapter
/// would actually meet — the point of driving real request bodies rather than a `GET`.
async fn offer(client: &reqwest::Client, target: &Target, width: usize, rate: u64) -> Arc<Counts> {
    let gate = Arc::new(tokio::sync::Semaphore::new(width));
    let counts = Arc::new(Counts::default());
    let deadline = tokio::time::Instant::now() + BLOCK;
    let interval = Duration::from_nanos(1_000_000_000 / rate.max(1));
    let mut handles = Vec::new();
    let mut n = 0_usize;
    while tokio::time::Instant::now() < deadline {
        let (client, gate, counts) = (client.clone(), Arc::clone(&gate), Arc::clone(&counts));
        let (method, url, body) = if n.is_multiple_of(2) {
            ("PROPFIND", target.home.clone(), PROPFIND)
        } else {
            ("REPORT", target.collection.clone(), REPORT)
        };
        let (user, pass) = (target.user.clone(), target.pass.clone());
        handles.push(tokio::spawn(async move {
            let _lane = gate.acquire().await.expect("semaphore open");
            let response = client
                .request(
                    reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
                    url,
                )
                .basic_auth(user, Some(pass))
                .header("Content-Type", "application/xml; charset=utf-8")
                .header("Depth", "1")
                .body(body)
                .send()
                .await;
            match response.map(|r| r.status().as_u16()) {
                Ok(200 | 207) => counts.ok.fetch_add(1, Ordering::SeqCst),
                Ok(429 | 503) => counts.refused.fetch_add(1, Ordering::SeqCst),
                _ => counts.other.fetch_add(1, Ordering::SeqCst),
            };
        }));
        n += 1;
        tokio::time::sleep(interval).await;
    }
    for handle in handles {
        handle.await.expect("request task");
    }
    counts
}

async fn measure(target: Target) {
    // Built through the shared TLS config, like every adapter's transport: it installs the
    // rustls crypto provider `rustls-no-provider` requires, and it means this probe sends
    // over the same client stack the adapter does.
    let client = engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .timeout(Duration::from_mins(1))
        .build()
        .expect("client");

    // --- Arm A: fixed rate, varying width. ---
    eprintln!("\n=== {} — A: 25/s, varying width ===", target.name);
    for width in [1_usize, 4, 16, 64] {
        tokio::time::sleep(COOLDOWN).await;
        let counts = offer(&client, &target, width, 25).await;
        eprintln!(
            "  width {width:>3}: {} ok, {} refused, {} other ({:.1}% refused)",
            counts.ok.load(Ordering::SeqCst),
            counts.refused.load(Ordering::SeqCst),
            counts.other.load(Ordering::SeqCst),
            counts.refused_percent(),
        );
        assert_eq!(
            counts.other.load(Ordering::SeqCst),
            0,
            "{}: a status that is neither success nor a refusal means the probe is malformed, \
             and a malformed probe cannot measure a ceiling",
            target.name,
        );
        assert!(
            counts.refused_percent() < 1.0,
            "{}: width {width} drew {:.1}% refusals at a rate that is clean at width 1 — \
             this server DOES bound concurrency, and `provider-caldav` should narrow the \
             account gate (`http-throttling.md`)",
            target.name,
            counts.refused_percent(),
        );
    }

    // --- Arm B: fixed width, varying rate. What makes Arm A mean something. ---
    eprintln!("=== {} — B: width 8, varying rate ===", target.name);
    let mut worst = 0.0_f64;
    for rate in [10_u64, 50, 150, 400] {
        tokio::time::sleep(COOLDOWN).await;
        let counts = offer(&client, &target, 8, rate).await;
        eprintln!(
            "  {rate:>4}/s: {} ok, {} refused ({:.1}% refused)",
            counts.ok.load(Ordering::SeqCst),
            counts.refused.load(Ordering::SeqCst),
            counts.refused_percent(),
        );
        worst = worst.max(counts.refused_percent());
    }
    eprintln!(
        "  → {}: width does not bound; rate {} (worst refusal share {:.1}%)",
        target.name,
        if worst > 1.0 {
            "does"
        } else {
            "did not either, up to 400/s"
        },
        worst,
    );
}

/// Stalwart: the arm that carries the proof, because it is the one that refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_stalwart_bounds_rate_and_not_concurrency() {
    let Some(target) = stalwart() else {
        eprintln!(
            "skipping live_stalwart_bounds_rate_and_not_concurrency: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    measure(target).await;
}

/// SabreDAV: a second implementation, so "CalDAV does not bound concurrency" rests on more
/// than one server's habits. It refuses nothing, which is why it cannot carry the proof alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_sabredav_bounds_neither() {
    let Some(target) = sabredav() else {
        eprintln!("skipping live_sabredav_bounds_neither: SABREDAV_HTTP_ADDR unset");
        return;
    };
    measure(target).await;
}
