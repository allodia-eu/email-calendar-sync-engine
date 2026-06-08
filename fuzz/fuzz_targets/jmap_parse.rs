#![no_main]

//! Fuzz the JMAP JSON parse + normalize pipeline: arbitrary bytes go through
//! `serde_json` and every `provider-jmap` normalizer. The invariant under test is
//! panic-freedom — hostile mail/calendar input must surface as an error, never a
//! crash (`north-star.md` security).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    provider_jmap::fuzz_parse(data);
});
