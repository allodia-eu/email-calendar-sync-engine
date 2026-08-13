//! The engine's mail baseline, measured over a fixture of known size.
//!
//! Seven operations, chosen because each one is on a path a user waits on:
//!
//! | Benchmark | What waits on it |
//! |---|---|
//! | `read/first_page` | the message list painting after launch |
//! | `read/deep_window` | the list a host keeps behind the visible rows |
//! | `read/thread_expansion` | completing every shown conversation |
//! | `apply/flag_only` | marking one message read |
//! | `apply/page` | a page of a sync landing |
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
use std::{cell::Cell, collections::HashSet, hint::black_box, path::Path, time::Instant};

use criterion::{BenchmarkGroup, Criterion, measurement::WallTime};
use engine_api::{AccountId, Engine, Keyword, Message, SystemKeyword};
use mailbox_fixture::{Fixture, FixtureSpec, Pass, Recorder, Scale, populate, sync_folder};
use tokio::runtime::Runtime;

/// The window a host paints on launch.
const FIRST_PAGE: usize = 100;

/// The window a host keeps loaded behind the visible rows.
const DEEP_WINDOW: usize = 2_000;

/// Messages in the page an incremental sync commits.
const PAGE: usize = 500;

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
    let page = runtime
        .block_on(engine.messages_windowed(account, FIRST_PAGE))
        .expect("read the first page");
    let threads: HashSet<String> = page
        .iter()
        .filter_map(|message| message.thread_id().map(|id| id.as_str().to_owned()))
        .collect();
    let shown: HashSet<String> = page
        .iter()
        .map(|message| message.id.key().as_str().to_owned())
        .collect();

    let mut fast = group(criterion, "read", FAST_SAMPLES);
    measure(&mut fast, recorder, "read/first_page", "first_page", || {
        black_box(
            runtime
                .block_on(engine.messages_windowed(account, FIRST_PAGE))
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
                    .block_on(engine.messages_windowed(account, DEEP_WINDOW))
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
                    .block_on(engine.thread_members(account, &threads, &shown))
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
    let mut slow = group(criterion, "open", SLOW_SAMPLES);
    measure(&mut slow, recorder, "open/cold", "cold", || {
        let engine = Engine::open(path).expect("re-open the fixture store");
        black_box(
            runtime
                .block_on(engine.messages_windowed(account, FIRST_PAGE))
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
