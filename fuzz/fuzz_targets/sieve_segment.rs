#![no_main]

//! Fuzz `engine-sieve`: arbitrary bytes go through the segmenter, and every string value read
//! is written back. The invariants under test are panic-freedom (a script is hostile input, RFC
//! 9661 §5) and the crate's promises:
//!
//! - when a script splits, each command segment split on its own is that one command again
//!   (that the segments join back to the script holds by construction, so it is no test);
//! - a value written as a quoted string, or as a `text:` block where one can hold it, reads back
//!   as itself.

use engine_sieve::{SegmentKind, segment, segment_bytes, string, tokenize};
use libfuzzer_sys::fuzz_target;

fn written_values_read_back(value: &str) {
    if let Some(quoted) = string::quoted(value) {
        let tokens = tokenize(&quoted).expect("a quoted value tokenizes");
        assert_eq!(tokens.len(), 1);
        assert_eq!(string::value(&tokens[0]).as_deref(), Some(value));
    }
    if let Some(block) = string::multi_line(value) {
        let tokens = tokenize(&block).expect("a text: block tokenizes");
        assert_eq!(tokens.len(), 1);
        assert_eq!(string::value(&tokens[0]).as_deref(), Some(value));
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = core::str::from_utf8(data) {
        written_values_read_back(text);
    }
    let Ok(segments) = segment_bytes(data) else {
        return;
    };
    for part in segments
        .iter()
        .filter(|segment| segment.kind() != SegmentKind::Trailing)
    {
        let alone = segment(part.text()).expect("a segment splits on its own");
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].text(), part.text());
    }
    let _ = engine_sieve::require::required(&segments);
    for token in segments.iter().flat_map(|segment| segment.tokens()) {
        if let Some(value) = string::value(token) {
            written_values_read_back(&value);
        }
    }
});
