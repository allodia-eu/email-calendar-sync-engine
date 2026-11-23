#![no_main]

//! Fuzz the Sieve segmenter: arbitrary bytes go through `engine_sieve::segment_bytes`, and
//! every segment's `require` names and string values are read. The invariants under test are
//! panic-freedom (a script is hostile input, RFC 9661 §5) and the segmenter's promise: when a
//! script splits, its segments' texts in order are its bytes.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(segments) = engine_sieve::segment_bytes(data) else {
        return;
    };
    let joined: Vec<u8> = segments
        .iter()
        .flat_map(|segment| segment.text().bytes())
        .collect();
    assert_eq!(joined, data);
    let _ = engine_sieve::require::required(&segments);
    for token in segments.iter().flat_map(|segment| segment.tokens()) {
        let _ = engine_sieve::string::value(token);
    }
});
