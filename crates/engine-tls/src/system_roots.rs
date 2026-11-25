//! The OS trust store, read at most once per [`REREAD_AFTER`] (the `system` root source of
//! [`TlsPolicy::Roots`](crate::TlsPolicy::Roots)).
//!
//! A read is `rustls-native-certs`'s: the certificate files under the OpenSSL paths on Linux and
//! the BSDs (or `SSL_CERT_FILE` and `SSL_CERT_DIR`, read again at each read), the Keychain and its
//! trust settings on macOS, the `ROOT` store on Windows. It opens and parses every certificate
//! (243 on one Linux machine: about 7 ms idle, up to over a second under load), which a host
//! building a config per connection or per request would otherwise pay on every one. So a read
//! serves the configs built after it for [`REREAD_AFTER`], and the first config built after that
//! reads again, on its own thread, as every config did before (`docs/agent-guidance/tls.md`).

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, TrustAnchor},
};

/// How long a read of the OS store serves the configs built after it: ten minutes.
///
/// It bounds both directions of a change to the OS store: an anchor an administrator adds is
/// trusted, and one removed or distrusted (a compromised root pulled by an update, a CA set to
/// never be trusted, a root an MDM removes) stops being trusted, by the configs built at most ten
/// minutes later, where before the next config honoured it. Ten minutes keeps the speed-up: a
/// host building a config per connection reads the store at most six times an hour, not once per
/// connection, while a removal reaches it well within the time an update takes to roll out.
pub(crate) const REREAD_AFTER: Duration = Duration::from_mins(10);

/// How long a read that found no anchor serves the configs built after it: five seconds.
///
/// Long enough that builders arriving together (a host reconnecting its accounts) share one
/// read of a store that fails slowly (a Keychain or `ROOT` store that hangs and returns nothing)
/// rather than each paying it in turn; short enough that a store unreadable for a moment is
/// read again a few seconds later, not ten minutes.
pub(crate) const UNREAD_RETRY_AFTER: Duration = Duration::from_secs(5);

/// The OS store's anchors, parsed.
pub(crate) fn system_roots() -> Arc<[TrustAnchor<'static>]> {
    static CACHE: Cache = Cache::new();
    CACHE.anchors(|| (Instant::now(), SystemTime::now()), load)
}

/// Reads the OS store: the anchors that parsed. The read's errors (a file that could not be
/// read, a certificate that would not parse) are dropped, as they always were; what decides
/// whether a read is kept is whether it found anchors ([`Cache::anchors`]).
fn load() -> Vec<TrustAnchor<'static>> {
    parse(rustls_native_certs::load_native_certs().certs).0
}

/// The anchors among `certs` that parse, in order and with any duplicate kept, and how many did
/// not parse, which are skipped, as [`RootCertStore::add_parsable_certificates`] skips them.
fn parse(certs: Vec<CertificateDer<'static>>) -> (Vec<TrustAnchor<'static>>, usize) {
    let mut store = RootCertStore::empty();
    let (_, unparsable) = store.add_parsable_certificates(certs);
    (store.roots, unparsable)
}

/// The anchors the last read kept, when it was made by both clocks, and for how long it
/// serves.
struct Kept {
    anchors: Arc<[TrustAnchor<'static>]>,
    read_at: Instant,
    read_at_wall: SystemTime,
    serves: Duration,
}

impl Kept {
    /// Whether this read still serves at `now` and `wall`: by both clocks younger than it
    /// serves for. The monotonic clock stops while the machine sleeps (`CLOCK_MONOTONIC` on
    /// Linux and Android, an uptime clock on macOS and iOS), so a read taken a minute before a
    /// night's sleep would look a minute old on waking, when a host reconnects its accounts; the
    /// wall clock counts the sleep. A wall clock set back says nothing about the read's age, so
    /// the read is stale then too.
    fn serves_at(&self, now: Instant, wall: SystemTime) -> bool {
        now.saturating_duration_since(self.read_at) < self.serves
            && wall
                .duration_since(self.read_at_wall)
                .is_ok_and(|age| age < self.serves)
    }
}

/// The last read worth keeping.
pub(crate) struct Cache(Mutex<Option<Kept>>);

impl Cache {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// The anchors as of `now` and `wall`: the kept read's while it serves
    /// ([`Kept::serves_at`]), and otherwise a new read's through `load`.
    ///
    /// A read that found anchors serves for [`REREAD_AFTER`], whatever errors it met beside them:
    /// a clean read is the store as it is, and one that met errors (a file the OS keeps but no
    /// one can read) would most likely meet them again on every config; a passing error that cost
    /// an anchor costs it for one interval at most. A read that found no anchor serves for
    /// [`UNREAD_RETRY_AFTER`] only: a store unreadable for a moment does not leave `system_only`
    /// configs without roots, or a union without its OS anchors, for ten minutes, and builders
    /// arriving together share one read of a store that fails slowly.
    ///
    /// The lock is held across the read, and the clocks (`clock`, the monotonic and the wall
    /// clock) are read once it is held, so builders that arrive while a read runs wait for it and
    /// share its anchors: after the interval, many configs built at once (a host reconnecting
    /// its accounts after a sleep) cost one read, not one each, under the load that makes a read
    /// slow.
    pub(crate) fn anchors(
        &self,
        clock: impl FnOnce() -> (Instant, SystemTime),
        load: impl FnOnce() -> Vec<TrustAnchor<'static>>,
    ) -> Arc<[TrustAnchor<'static>]> {
        // A read that panicked poisoned the lock and kept nothing; the next caller reads again.
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // The clocks are read with the lock held, so the times callers see follow the order they
        // get the lock in: one that read the time before another but got the lock after it
        // would hold an earlier wall time than the read the other kept, which looks like a wall
        // clock set back, and would read again instead of sharing.
        let (now, wall) = clock();
        if let Some(fresh) = kept.as_ref().filter(|kept| kept.serves_at(now, wall)) {
            return Arc::clone(&fresh.anchors);
        }
        let anchors: Arc<[TrustAnchor<'static>]> = load().into();
        let serves = if anchors.is_empty() {
            UNREAD_RETRY_AFTER
        } else {
            REREAD_AFTER
        };
        *kept = Some(Kept {
            anchors: Arc::clone(&anchors),
            read_at: now,
            read_at_wall: wall,
            serves,
        });
        anchors
    }
}

#[cfg(test)]
#[path = "system_roots_tests.rs"]
mod tests;
