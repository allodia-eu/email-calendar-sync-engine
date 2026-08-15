//! The engine's mail baseline, measured over a fixture of known size.
//!
//! Seven operations, chosen because each one is on a path a user waits on:
//!
//! | Benchmark | What waits on it |
//! |---|---|
//! | `read/first_page` | the message list painting after launch |
//! | `read/deep_window` | the list a host keeps behind the visible rows |
//! | `read/thread_expansion` | completing every shown conversation |
//! | `apply/flag_only` | marking one message read, as a whole object |
//! | `apply/state_only` | marking one message read, as the state change it is |
//! | `apply/page` | a page of a sync landing |
//! | `mixed/read_under_apply` | the list, read while a sync commits |
//! | `open/cold` | opening the store and painting the first rows |
//! | `threads/derive` | the pass that runs after **every** account sync |
//!
//! Size comes from `ENGINE_BENCH_SCALE` (`10k` by default, `100k` in CI, `400k`
//! opt-in). The fixture is built once per process, on disk, because a cold open is one
//! of the things being measured and an in-memory store cannot be re-opened.
//!
//! Beside criterion's own statistics the run prints an `n / p50 / p90 / p99 / max`
//! table, which is the shape a host reduces its own logged durations to — so the same
//! operation can be read here against a fixture of known size and there against
//! whatever mail a user actually has. Every iteration criterion runs is recorded,
//! warm-up included, and `n` says how far to trust the p99 on its row: the cheap
//! operations take a hundred samples, the ones that take seconds take ten.
//!
//! ```sh
//! cargo bench -p mailbox-fixture                       # 10k
//! ENGINE_BENCH_SCALE=100k cargo bench -p mailbox-fixture
//! ```

use core::time::Duration;
use std::{cell::Cell, collections::BTreeSet, hint::black_box, path::Path, time::Instant};

use criterion::{BenchmarkGroup, Criterion, measurement::WallTime};
use engine_api::{AccountId, Engine, Keyword, Message, SystemKeyword};
use engine_core::mail::MailStateChange;
use mailbox_fixture::{Fixture, FixtureSpec, Pass, Recorder, Scale, populate, sync_folder};
use tokio::runtime::Runtime;

/// The window a host paints on launch.
const FIRST_PAGE: usize = 100;

/// The window a host keeps loaded behind the visible rows.
const DEEP_WINDOW: usize = 2_000;

/// Messages in the page an incremental sync commits.
const PAGE: usize = 500;

/// List reads issued at once against the page that is committing.
const CONCURRENT_READS: usize = 4;

/// Samples for an operation that costs milliseconds — enough for the p99 to mean
/// something.
const FAST_SAMPLES: usize = 100;

/// Samples for an operation that costs a large fraction of a second or more. Ten is
/// criterion's floor; the `n` column reports it so nobody reads the p99 as more than
/// it is.
const SLOW_SAMPLES: usize = 10;

fn main() {
    let scale = Scale::from_env();
    let runtime = Runtime::new().expect("a tokio runtime");
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("scale.sqlite");
    let account = AccountId::try_from("bench-account").expect("a valid account id");
    let spec = FixtureSpec::new(account.clone(), scale.messages);

    let engine = Engine::open(&path).expect("open the fixture store");
    let started = Instant::now();
    let fixture = runtime
        .block_on(populate(&engine, &spec))
        .expect("populate the fixture");
    eprintln!(
        "fixture: {} messages across {} folders, built in {:.1}s",
        fixture.len(),
        fixture.folders.len(),
        started.elapsed().as_secs_f64()
    );

    let recorder = Recorder::new();
    let mut criterion = Criterion::default().configure_from_args();
    reads(&mut criterion, &recorder, &runtime, &engine, &account);
    applies(
        &mut criterion,
        &recorder,
        &runtime,
        &engine,
        &spec,
        &fixture,
    );
    contended(
        &mut criterion,
        &recorder,
        &runtime,
        &engine,
        &spec,
        &fixture,
        &account,
    );
    cold_open(&mut criterion, &recorder, &runtime, &path, &account);
    derive(&mut criterion, &recorder, &runtime, &engine, &account);

    criterion.final_summary();
    println!("{}", recorder.table(scale.label));
}

/// Times `body` once per criterion iteration, recording every measurement.
///
/// `iter_custom` rather than `iter` because the table needs the individual durations,
/// not the batch total criterion would otherwise hand back.
fn measure<F: FnMut()>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    recorder: &Recorder,
    operation: &str,
    name: &str,
    mut body: F,
) {
    group.bench_function(name, |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let started = Instant::now();
                body();
                let elapsed = started.elapsed();
                recorder.record(operation, elapsed);
                total += elapsed;
            }
            total
        });
    });
}

/// Configures a group: criterion's defaults assume a microbenchmark, and these are
/// not.
fn group<'a>(
    criterion: &'a mut Criterion,
    name: &str,
    samples: usize,
) -> BenchmarkGroup<'a, WallTime> {
    let mut group = criterion.benchmark_group(name);
    group.sample_size(samples);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(if samples == FAST_SAMPLES {
        10
    } else {
        20
    }));
    group
}

/// The list reads: the first page, the deep window behind it, and completing the
/// conversations the first page shows.
fn reads(
    criterion: &mut Criterion,
    recorder: &Recorder,
    runtime: &Runtime,
    engine: &Engine,
    account: &AccountId,
) {
    // Every list read takes the accounts it spans, so one account is a one-element slice.
    let accounts = core::slice::from_ref(account);
    let page = runtime
        .block_on(engine.mail_window(accounts, FIRST_PAGE))
        .expect("read the first page");
    let threads: Vec<String> = page
        .iter()
        .filter_map(|row| row.mail.thread_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();

    let mut fast = group(criterion, "read", FAST_SAMPLES);
    measure(&mut fast, recorder, "read/first_page", "first_page", || {
        black_box(
            runtime
                .block_on(engine.mail_window(accounts, FIRST_PAGE))
                .expect("read the first page"),
        );
    });
    fast.finish();

    let mut slow = group(criterion, "read_deep", SLOW_SAMPLES);
    measure(
        &mut slow,
        recorder,
        "read/deep_window",
        "deep_window",
        || {
            black_box(
                runtime
                    .block_on(engine.mail_window(accounts, DEEP_WINDOW))
                    .expect("read the deep window"),
            );
        },
    );
    measure(
        &mut slow,
        recorder,
        "read/thread_expansion",
        "thread_expansion",
        || {
            black_box(
                runtime
                    .block_on(engine.mail_on_threads(accounts, threads.iter().map(String::as_str)))
                    .expect("complete the shown conversations"),
            );
        },
    );
    slow.finish();
}

/// The write path: one message's keyword changing, and a page of a sync landing.
fn applies(
    criterion: &mut Criterion,
    recorder: &Recorder,
    runtime: &Runtime,
    engine: &Engine,
    spec: &FixtureSpec,
    fixture: &Fixture,
) {
    let folder = busiest(fixture);
    let source = &fixture.folders[folder].messages;
    // Two versions of one message, applied alternately, so each iteration really is a
    // keyword change rather than a re-write of bytes that already match.
    let read = with_seen(&source[0], true);
    let unread = with_seen(&source[0], false);
    let toggle = Cell::new(false);
    let page: Vec<Message> = source.iter().take(PAGE).cloned().collect();

    let mut fast = group(criterion, "apply", FAST_SAMPLES);
    measure(&mut fast, recorder, "apply/flag_only", "flag_only", || {
        toggle.set(!toggle.get());
        let message = if toggle.get() { &read } else { &unread };
        black_box(
            runtime
                .block_on(sync_folder(
                    engine,
                    spec,
                    fixture,
                    folder,
                    Pass::Delta(vec![message.clone()]),
                ))
                .expect("apply a flag-only delta"),
        );
    });
    // The same user action — one message marked read — as the state change every adapter
    // emits now. Beside `apply/flag_only` on purpose: that is what the same action cost when
    // it had to arrive as a whole object, and the pair is the measurement.
    let key = source[0].id.key().clone();
    let seen: BTreeSet<Keyword> = [Keyword::system(SystemKeyword::Seen)].into_iter().collect();
    let state_toggle = Cell::new(false);
    measure(
        &mut fast,
        recorder,
        "apply/state_only",
        "state_only",
        || {
            state_toggle.set(!state_toggle.get());
            let keywords = if state_toggle.get() {
                seen.clone()
            } else {
                BTreeSet::new()
            };
            black_box(
                runtime
                    .block_on(sync_folder(
                        engine,
                        spec,
                        fixture,
                        folder,
                        Pass::State(vec![MailStateChange::keywords(key.clone(), keywords)]),
                    ))
                    .expect("apply a state-only delta"),
            );
        },
    );
    fast.finish();

    let mut slow = group(criterion, "apply_page", SLOW_SAMPLES);
    measure(&mut slow, recorder, "apply/page", "page", || {
        black_box(
            runtime
                .block_on(sync_folder(
                    engine,
                    spec,
                    fixture,
                    folder,
                    Pass::Delta(page.clone()),
                ))
                .expect("apply a page"),
        );
    });
    slow.finish();
}

/// Reads issued while a sync page commits — what a user actually meets.
///
/// The other read benches have the store to themselves, which is the one condition a
/// mail client is never in: a list is painted *while* mail is arriving. This times one
/// page apply and [`CONCURRENT_READS`] first-page reads started together, so the number
/// says whether they overlapped or queued. Serialized behind one connection it is the
/// sum; over a writer plus a reader pool it is close to the slower of the two.
#[allow(
    clippy::too_many_arguments,
    reason = "the contended case needs both halves of the store's workload"
)]
fn contended(
    criterion: &mut Criterion,
    recorder: &Recorder,
    runtime: &Runtime,
    engine: &Engine,
    spec: &FixtureSpec,
    fixture: &Fixture,
    account: &AccountId,
) {
    let accounts = core::slice::from_ref(account);
    let folder = busiest(fixture);
    let page: Vec<Message> = fixture.folders[folder]
        .messages
        .iter()
        .take(PAGE)
        .cloned()
        .collect();

    let mut slow = group(criterion, "mixed", SLOW_SAMPLES);
    measure(
        &mut slow,
        recorder,
        "mixed/read_under_apply",
        "read_under_apply",
        || {
            runtime.block_on(async {
                let apply = sync_folder(engine, spec, fixture, folder, Pass::Delta(page.clone()));
                let reads = futures_util::future::join_all(
                    (0..CONCURRENT_READS).map(|_| engine.mail_window(accounts, FIRST_PAGE)),
                );
                let (applied, read) = futures_util::future::join(apply, reads).await;
                black_box(applied.expect("apply a page while reading"));
                for rows in read {
                    black_box(rows.expect("read the first page under load"));
                }
            });
        },
    );
    slow.finish();
}

/// Opening the store and painting the first rows — what a launch costs.
///
/// The database file is already in the host's page cache by the time this runs, so
/// this is the relaunch case rather than a first-ever-boot one. That is the case a
/// user meets most often, and the one the sub-second budget is written against.
fn cold_open(
    criterion: &mut Criterion,
    recorder: &Recorder,
    runtime: &Runtime,
    path: &Path,
    account: &AccountId,
) {
    let accounts = core::slice::from_ref(account);
    let mut slow = group(criterion, "open", SLOW_SAMPLES);
    measure(&mut slow, recorder, "open/cold", "cold", || {
        let engine = Engine::open(path).expect("re-open the fixture store");
        black_box(
            runtime
                .block_on(engine.mail_window(accounts, FIRST_PAGE))
                .expect("paint the first page"),
        );
    });
    slow.finish();
}

/// The thread-derivation pass that follows every account sync.
///
/// The fixture's mail already carries the ids derivation would assign, so this
/// measures the steady state: a full read of every message in every folder that then
/// writes nothing. That is what the pass costs on a mailbox where nothing changed —
/// which is most of the times it runs.
fn derive(
    criterion: &mut Criterion,
    recorder: &Recorder,
    runtime: &Runtime,
    engine: &Engine,
    account: &AccountId,
) {
    let mut slow = group(criterion, "threads", SLOW_SAMPLES);
    measure(&mut slow, recorder, "threads/derive", "derive", || {
        black_box(
            runtime
                .block_on(engine.derive_mail_threads(account))
                .expect("derive thread ids"),
        );
    });
    slow.finish();
}

/// The index of the folder holding the most mail — the one a write bench should aim
/// at, and the only one guaranteed non-empty at any fixture size.
fn busiest(fixture: &Fixture) -> usize {
    fixture
        .folders
        .iter()
        .enumerate()
        .max_by_key(|(_, folder)| folder.messages.len())
        .map(|(index, _)| index)
        .expect("a fixture has folders")
}

/// The same message with its `$seen` keyword set or cleared.
fn with_seen(message: &Message, seen: bool) -> Message {
    let mut copy = message.clone();
    let keyword = Keyword::system(SystemKeyword::Seen);
    if seen {
        copy.keywords.insert(keyword);
    } else {
        copy.keywords.remove(&keyword);
    }
    copy
}
