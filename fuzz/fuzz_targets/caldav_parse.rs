#![no_main]

//! Fuzz the iCalendar parse + normalize pipeline: arbitrary bytes go through
//! unfolding, the component tree, and every `provider-caldav` normalizer into an
//! `Event`. The invariant under test is panic-freedom — hostile calendar input
//! must surface as an error, never a crash (`north-star.md` security).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    provider_caldav::fuzz_parse(data);
});
