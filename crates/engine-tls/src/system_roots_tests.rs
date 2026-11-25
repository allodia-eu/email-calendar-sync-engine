//! The OS store's cache, one per process: a read that found anchors serves for [`REREAD_AFTER`] by
//! both the monotonic and the wall clock, after which the next config reads again (so a removal
//! takes effect, as an addition does, a sleep included); a read that found none serves for
//! [`UNREAD_RETRY_AFTER`]; a panicking read leaves the cache usable; and the anchors a read
//! keeps are the ones a fresh read gives, with garbage skipped and duplicates kept.

use std::{
    cell::Cell,
    time::{Duration, Instant, SystemTime},
};

use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, TrustAnchor},
};

use super::{Cache, REREAD_AFTER, UNREAD_RETRY_AFTER, parse, system_roots};
use crate::config::{assemble, build_root_store};

fn der() -> CertificateDer<'static> {
    rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("a self-signed certificate")
        .cert
        .der()
        .clone()
}

/// A read finding `count` copies of one anchor.
fn read(count: usize) -> Vec<TrustAnchor<'static>> {
    parse(vec![der(); count]).0
}

/// A moment by both clocks the cache reads: the monotonic one, which stops while the machine
/// sleeps, and the wall clock, which does not.
#[derive(Clone, Copy)]
struct At {
    instant: Instant,
    wall: SystemTime,
}

impl At {
    fn start() -> Self {
        Self {
            instant: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// `elapsed` later, awake: both clocks move.
    fn after(self, elapsed: Duration) -> Self {
        Self {
            instant: self.instant + elapsed,
            wall: self.wall + elapsed,
        }
    }

    /// `slept` later by the wall clock, after a sleep the monotonic clock did not count.
    fn after_sleeping(self, slept: Duration) -> Self {
        Self {
            wall: self.wall + slept,
            ..self
        }
    }
}

/// The anchors `cache` serves at `at`, and whether it read the store for them.
fn ask(cache: &Cache, at: At, then: impl FnOnce() -> Vec<TrustAnchor<'static>>) -> (usize, bool) {
    let loaded = Cell::new(false);
    let anchors = cache.anchors(
        || (at.instant, at.wall),
        || {
            loaded.set(true);
            then()
        },
    );
    (anchors.len(), loaded.get())
}

#[test]
fn a_read_serves_until_the_interval_passes_and_then_the_store_is_read_again() {
    let cache = Cache::new();
    let start = At::start();
    assert_eq!(ask(&cache, start, || read(3)), (3, true));
    let within = start.after(REREAD_AFTER.saturating_sub(Duration::from_secs(1)));
    assert_eq!(ask(&cache, within, || read(1)), (3, false));
    // An anchor removed from the store is gone from the configs built after the interval.
    let after = start.after(REREAD_AFTER);
    assert_eq!(ask(&cache, after, || read(2)), (2, true));
    assert_eq!(
        ask(&cache, after.after(Duration::from_secs(1)), || read(9)),
        (2, false)
    );
}

/// The monotonic clock stops while the machine sleeps, so a read taken a minute before a long
/// sleep would look a minute old on waking, when a host reconnects its accounts. The wall clock
/// counts the sleep: the read is stale.
#[test]
fn a_sleep_longer_than_the_interval_makes_the_read_stale() {
    let cache = Cache::new();
    let start = At::start();
    assert_eq!(ask(&cache, start, || read(3)), (3, true));
    let woken = start
        .after(Duration::from_mins(1))
        .after_sleeping(Duration::from_hours(8));
    assert_eq!(ask(&cache, woken, || read(2)), (2, true));
}

/// A wall clock set back (by hand, or by a time sync after a drift) says nothing about how old
/// the read is, so the read is stale.
#[test]
fn a_wall_clock_set_back_makes_the_read_stale() {
    let cache = Cache::new();
    let start = At::start();
    assert_eq!(ask(&cache, start, || read(3)), (3, true));
    let set_back = At {
        instant: start.instant + Duration::from_secs(1),
        wall: start.wall - Duration::from_secs(1),
    };
    assert_eq!(ask(&cache, set_back, || read(2)), (2, true));
}

/// A read that found no anchor serves only builders that arrive within
/// [`UNREAD_RETRY_AFTER`]: they share it rather than each pay a store that fails slowly, and a
/// store unreadable for a moment is read again a few seconds later, not ten minutes.
#[test]
fn a_read_that_found_no_anchor_serves_a_few_seconds_and_then_the_store_is_read_again() {
    let cache = Cache::new();
    let start = At::start();
    assert_eq!(ask(&cache, start, || read(0)), (0, true));
    let within = start.after(UNREAD_RETRY_AFTER.saturating_sub(Duration::from_millis(1)));
    assert_eq!(ask(&cache, within, || read(3)), (0, false));
    let after = start.after(UNREAD_RETRY_AFTER);
    assert_eq!(ask(&cache, after, || read(3)), (3, true));
    assert_eq!(
        ask(&cache, after.after(Duration::from_mins(5)), || read(1)),
        (3, false)
    );
}

/// A read that found anchors serves for the interval whatever errors it met beside them (one
/// unreadable file the OS keeps, say), so such a machine does not read on every config; the
/// next read replaces it once the interval has passed.
#[test]
fn a_partial_read_serves_for_the_interval_and_no_longer() {
    let cache = Cache::new();
    let start = At::start();
    assert_eq!(ask(&cache, start, || read(2)), (2, true));
    assert_eq!(
        ask(&cache, start.after(Duration::from_mins(1)), || read(3)),
        (2, false)
    );
    assert_eq!(
        ask(&cache, start.after(REREAD_AFTER), || read(3)),
        (3, true)
    );
}

/// A read that panics poisons the lock; the next config reads again and its read is kept.
#[test]
fn a_read_that_panicked_leaves_the_cache_usable() {
    let cache = Cache::new();
    let start = At::start();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cache.anchors(
            || (start.instant, start.wall),
            || panic!("the store read panicked"),
        )
    }));
    assert!(panicked.is_err());
    assert_eq!(ask(&cache, start, || read(2)), (2, true));
    assert_eq!(ask(&cache, start, || read(5)), (2, false));
}

/// Garbage is skipped, and a duplicate is kept as a duplicate, in order: the OS store's
/// anchors follow the bundled ones whatever they repeat, as `add_parsable_certificates` and
/// `extend` keep them.
#[test]
fn garbage_is_skipped_and_duplicates_are_kept_in_order() {
    let one = der();
    let garbage = CertificateDer::from(vec![0_u8, 1, 2, 3]);
    let (anchors, unparsable) = parse(vec![garbage, one.clone(), one]);
    assert_eq!(unparsable, 1);
    assert_eq!(anchors.len(), 2);
    assert_eq!(anchors[0], anchors[1]);

    let bundled = webpki_roots::TLS_SERVER_ROOTS.len();
    let first_bundled = webpki_roots::TLS_SERVER_ROOTS[0].clone();
    let store = assemble(true, std::slice::from_ref(&first_bundled), &[]).expect("roots");
    assert_eq!(store.len(), bundled + 1);
    assert_eq!(store.roots[0], first_bundled);
    assert_eq!(store.roots[bundled], first_bundled);
}

/// A config built from the kept anchors trusts what one built by reading the store does: the
/// bundled anchors, in order, then the OS store's parsable ones. The OS store's are compared as
/// a collection, duplicates counted: macOS hands its keychain certificates back in a different
/// order on every read, and the order of trust anchors does not change what a config trusts.
/// (Compared against this machine's store, which may be empty; the crafted cases above do not
/// depend on it.)
#[test]
fn the_kept_anchors_are_the_ones_a_fresh_read_gives() {
    let kept = build_root_store(true, true, &[]).expect("bundled and system");
    let mut fresh = RootCertStore::empty();
    let _ = fresh.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);

    let bundled = webpki_roots::TLS_SERVER_ROOTS.len();
    assert_eq!(kept.roots[..bundled], webpki_roots::TLS_SERVER_ROOTS[..]);
    assert_eq!(sorted(&kept.roots[bundled..]), sorted(&fresh.roots));
}

/// `anchors` in one fixed order, whatever order a read gave them in.
fn sorted<'a>(anchors: &[TrustAnchor<'a>]) -> Vec<TrustAnchor<'a>> {
    let mut anchors = anchors.to_vec();
    anchors.sort_by(|a, b| {
        (
            &*a.subject,
            &*a.subject_public_key_info,
            a.name_constraints.as_deref(),
        )
            .cmp(&(
                &*b.subject,
                &*b.subject_public_key_info,
                b.name_constraints.as_deref(),
            ))
    });
    anchors
}

/// Builders that arrive while a read runs wait for it and share it: after the interval, many
/// configs built at once cost one read, whether it found anchors or not.
#[test]
fn builders_at_once_share_one_read() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let reads = AtomicUsize::new(0);
    let start = At::start();
    for found in [2, 0] {
        reads.store(0, Ordering::SeqCst);
        let cache = Cache::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let anchors = cache.anchors(
                        || (start.instant, start.wall),
                        || {
                            reads.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(20));
                            read(found)
                        },
                    );
                    assert_eq!(anchors.len(), found);
                });
            }
        });
        // A read that found nothing is shared too: a store that fails slowly costs one read.
        assert_eq!(reads.load(Ordering::SeqCst), 1, "found {found}");
    }
}

/// The clocks are read once the lock is held, so a builder that read the time before another
/// but got the lock after it cannot hold an earlier time than the read the other kept, which
/// would look like a wall clock set back and cost a second read. Here builder A reads the time
/// first and is held back inside its clock read until B starts reading, for at most 100 ms: with
/// the clocks read before the lock, B would read first and A would read again; read under the
/// lock, B waits for A. One read serves both, for a read that found anchors and for one that
/// found none.
#[test]
fn a_builder_that_read_the_time_before_the_reader_shares_its_read() {
    use std::sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    };

    for found in [2, 0] {
        let cache = Cache::new();
        let start = At::start();
        let ticks = AtomicU64::new(0);
        let reads = AtomicUsize::new(0);
        // A clock that moves on a millisecond each time it is read, as a real one does.
        let tick = || {
            let at = start.after(Duration::from_millis(ticks.fetch_add(1, Ordering::SeqCst)));
            (at.instant, at.wall)
        };
        let (taken, a_has_the_time) = mpsc::channel();
        let (reading, b_is_reading) = mpsc::channel();
        let load = || {
            reads.fetch_add(1, Ordering::SeqCst);
            let _ = reading.send(());
            std::thread::sleep(Duration::from_millis(20));
            read(found)
        };
        let (cache, tick, load) = (&cache, &tick, &load);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                cache.anchors(
                    || {
                        let time = tick();
                        let _ = taken.send(());
                        // Until B is reading, or for a while if B cannot get in.
                        let _ = b_is_reading.recv_timeout(Duration::from_millis(100));
                        time
                    },
                    load,
                )
            });
            scope.spawn(move || {
                a_has_the_time.recv().expect("A read the time");
                cache.anchors(tick, load)
            });
        });
        assert_eq!(reads.load(Ordering::SeqCst), 1, "found {found}");
    }
}

/// The configs a process builds share one cache: a second call is served the read the first
/// one kept (the same allocation), not a read of its own. Both calls fall within one interval:
/// this test binary runs for well under the five seconds an empty read serves.
#[test]
fn the_process_keeps_one_read_for_every_config() {
    let first = system_roots();
    let second = system_roots();
    assert!(std::sync::Arc::ptr_eq(&first, &second));
}
