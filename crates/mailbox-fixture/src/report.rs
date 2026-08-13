//! Reducing raw timings to one comparable table.
//!
//! A benchmark harness reports central tendency; the numbers that decide whether a
//! mail list feels broken are the tail ones — the sync that took eight seconds, not
//! the median that took twenty milliseconds. So every sample is kept and reported as
//! `n / p50 / p90 / p99 / max`, which is also the shape a host reduces its own logged
//! durations to. The two tables can then be read side by side: the same operation,
//! measured here on a fixture of known size and there on whatever the user actually
//! has.

use core::time::Duration;
use std::{collections::BTreeMap, fmt::Write as _, sync::Mutex};

/// Collects every sample of every operation measured in one run.
///
/// Shared across benchmarks by reference and safe to record into from anywhere, so a
/// harness that times its own iterations can hand each duration over as it lands.
#[derive(Debug, Default)]
pub struct Recorder {
    samples: Mutex<BTreeMap<String, Vec<Duration>>>,
}

impl Recorder {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one measured duration for `operation`.
    pub fn record(&self, operation: &str, elapsed: Duration) {
        self.samples
            .lock()
            .expect("recorder mutex poisoned")
            .entry(operation.to_owned())
            .or_default()
            .push(elapsed);
    }

    /// Renders every operation recorded so far as a table, ordered by name.
    ///
    /// `label` names what the measurements are of — the fixture size, typically —
    /// and is printed above the table so an archived run says what it measured.
    #[must_use]
    pub fn table(&self, label: &str) -> String {
        let samples = self.samples.lock().expect("recorder mutex poisoned");
        let rows: Vec<Row> = samples
            .iter()
            .map(|(operation, samples)| Row::of(operation, samples))
            .collect();
        render(label, &rows)
    }
}

/// One operation's reduced timings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    operation: String,
    count: usize,
    p50: Duration,
    p90: Duration,
    p99: Duration,
    max: Duration,
}

impl Row {
    /// Reduces one operation's samples. Empty input yields a row of zeroes rather
    /// than being dropped, so an operation that ran but recorded nothing is visible
    /// instead of silently absent.
    fn of(operation: &str, samples: &[Duration]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        Self {
            operation: operation.to_owned(),
            count: sorted.len(),
            p50: percentile(&sorted, 50),
            p90: percentile(&sorted, 90),
            p99: percentile(&sorted, 99),
            max: sorted.last().copied().unwrap_or_default(),
        }
    }
}

/// The nearest-rank percentile of an ascending slice: the smallest sample at or above
/// `p` percent of the way through it.
///
/// Nearest-rank rather than an interpolating definition because every value it
/// reports is a duration that was actually observed — an interpolated p99 is a number
/// no run ever took.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Renders the rows as a fixed-width table.
fn render(label: &str, rows: &[Row]) -> String {
    let width = rows
        .iter()
        .map(|row| row.operation.len())
        .chain(core::iter::once("Operation".len()))
        .max()
        .unwrap_or_default();
    let mut out = format!("\n{label}\n\n");
    let _ = writeln!(
        out,
        "| {:<width$} | {:>7} | {:>10} | {:>10} | {:>10} | {:>10} |",
        "Operation", "n", "p50", "p90", "p99", "max"
    );
    let _ = writeln!(
        out,
        "|{:-<w1$}|{:-<9}|{:-<12}|{:-<12}|{:-<12}|{:-<12}|",
        "",
        "",
        "",
        "",
        "",
        "",
        w1 = width + 2
    );
    for row in rows {
        let _ = writeln!(
            out,
            "| {:<width$} | {:>7} | {:>10} | {:>10} | {:>10} | {:>10} |",
            row.operation,
            row.count,
            millis(row.p50),
            millis(row.p90),
            millis(row.p99),
            millis(row.max)
        );
    }
    out
}

/// Formats a duration in milliseconds, keeping enough decimals to stay informative
/// across the four orders of magnitude these operations span.
fn millis(value: Duration) -> String {
    let ms = value.as_secs_f64() * 1_000.0;
    if ms < 1.0 {
        format!("{ms:.3} ms")
    } else if ms < 100.0 {
        format!("{ms:.2} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{Recorder, Row, millis, percentile};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn percentiles_are_observed_values_not_interpolated_ones() {
        let sorted: Vec<Duration> = (1..=100).map(ms).collect();
        assert_eq!(percentile(&sorted, 50), ms(50));
        assert_eq!(percentile(&sorted, 90), ms(90));
        assert_eq!(percentile(&sorted, 99), ms(99));
        // Every reported number is a duration some run actually took.
        assert!(sorted.contains(&percentile(&sorted, 75)));
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let sorted = vec![ms(7)];
        for p in [50, 90, 99] {
            assert_eq!(percentile(&sorted, p), ms(7));
        }
        assert_eq!(percentile(&[], 50), Duration::ZERO, "no samples, no panic");
    }

    #[test]
    fn a_row_reports_the_tail_the_median_hides() {
        // The case the whole table exists for: a fast median next to one pathological
        // sample. A mean would bury it; p99 and max must not.
        let mut samples: Vec<Duration> = vec![ms(20); 99];
        samples.push(ms(14_763));
        let row = Row::of("rebuild_snapshot", &samples);
        assert_eq!(row.count, 100);
        assert_eq!(row.p50, ms(20));
        assert_eq!(row.p99, ms(20));
        assert_eq!(row.max, ms(14_763));
    }

    #[test]
    fn the_table_names_every_recorded_operation_once() {
        let recorder = Recorder::new();
        recorder.record("read/first_page", ms(3));
        recorder.record("read/first_page", ms(5));
        recorder.record("apply/flag_only", ms(1));
        let table = recorder.table("100k messages");

        assert!(
            table.contains("100k messages"),
            "the table says what it measured"
        );
        assert_eq!(table.matches("read/first_page").count(), 1);
        assert!(table.contains("apply/flag_only"));
        // Ordered by name, so two runs diff cleanly.
        let first = table.find("apply/flag_only").unwrap();
        let second = table.find("read/first_page").unwrap();
        assert!(first < second);
    }

    #[test]
    fn durations_keep_their_precision_across_the_range() {
        assert_eq!(millis(Duration::from_micros(420)), "0.420 ms");
        assert_eq!(millis(ms(25)), "25.00 ms");
        assert_eq!(millis(ms(14_763)), "14763 ms");
    }
}
