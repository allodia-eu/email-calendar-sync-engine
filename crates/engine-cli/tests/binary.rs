//! Smoke tests that drive the actual `engine-cli` binary as a subprocess, covering
//! the `main.rs` shim (success and error exit paths).

use std::process::Command;

use engine_core::{
    calendar::{Event, Frequency, Recurrence, RecurrenceRule},
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, LocalDateTime},
};

/// The `engine-cli` this `cargo test` built.
///
/// ⚠️ Not `env!("CARGO_BIN_EXE_engine-cli")` alone: that is resolved when the test is *compiled*,
/// and a compiler cache can hand this checkout a test binary compiled in another one, carrying the
/// other checkout's absolute path ([`.cargo/config.toml`](../../../.cargo/config.toml)). Cargo
/// exports the same variable into the test *process*, computed by the run happening now; the
/// compile-time value is the fallback for running the test executable outside cargo.
fn bin() -> Command {
    Command::new(
        std::env::var_os("CARGO_BIN_EXE_engine-cli")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_engine-cli").into()),
    )
}

#[test]
fn cargo_names_the_binary_at_run_time() {
    // What `bin` relies on: without it every run would fall back to the compiled-in path.
    let binary = std::env::var_os("CARGO_BIN_EXE_engine-cli").expect("exported by cargo");
    assert!(std::path::Path::new(&binary).exists());
}

#[test]
fn no_args_prints_usage_and_exits_nonzero() {
    let output = bin().output().expect("run binary");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
}

#[test]
fn ingest_then_search_via_binary() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    let db = db.to_str().unwrap();

    let mut event = Event::new(
        EventId::try_from("daily").unwrap(),
        Uid::new("u-daily").unwrap(),
        Memberships::of_one(CalendarId::try_from("cal").unwrap()),
        CalendarDateTime::utc(LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap()),
    );
    event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
    let fixture_path = dir.path().join("fixture.json");
    std::fs::write(
        &fixture_path,
        serde_json::to_string(&serde_json::json!({ "events": [event] })).unwrap(),
    )
    .unwrap();

    let ingest = bin()
        .args([
            "ingest",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-01",
            "--horizon-end",
            "2026-06-05",
            fixture_path.to_str().unwrap(),
        ])
        .output()
        .expect("run ingest");
    assert!(ingest.status.success());
    assert!(String::from_utf8_lossy(&ingest.stdout).contains("4 occurrences"));

    let search = bin()
        .args([
            "search",
            "--db",
            db,
            "--account",
            "acct-1",
            "--kind",
            "calendar",
            "after:2026-06-01 before:2026-06-05",
        ])
        .output()
        .expect("run search");
    assert!(search.status.success());
    assert!(String::from_utf8_lossy(&search.stdout).contains("daily"));
}
