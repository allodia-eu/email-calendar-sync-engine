//! OpenPGP's four-octet times (RFC 9580 §3.5) as engine instants.

use engine_core::time::UtcDateTime;

/// The instant `seconds` after the Unix epoch.
pub(crate) fn instant(seconds: u32) -> UtcDateTime {
    instant_at(i64::from(seconds))
}

/// The instant `seconds` after the Unix epoch, for sums of four-octet times, which
/// stay far inside the representable years.
pub(crate) fn instant_at(seconds: i64) -> UtcDateTime {
    UtcDateTime::from_unix_seconds(seconds).expect("an OpenPGP time is a representable instant")
}
