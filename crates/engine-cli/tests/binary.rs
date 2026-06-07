//! Smoke tests that drive the actual `engine-cli` binary as a subprocess, covering
//! the `main.rs` shim (success and error exit paths).

use std::process::Command;

use engine_core::calendar::{Event, Frequency, Recurrence, RecurrenceRule};
use engine_core::ids::{CalendarId, EventId, Uid};
use engine_core::membership::Memberships;
use engine_core::time::{CalendarDateTime, LocalDateTime};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_engine-cli"))
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
