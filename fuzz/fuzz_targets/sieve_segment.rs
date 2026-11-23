#![no_main]

//! Fuzz `engine-sieve`: arbitrary bytes go through the segmenter, and every string value read
//! is written back. The invariants under test are panic-freedom (a script is hostile input, RFC
//! 9661 §5) and the crate's promises:
//!
//! - when a script splits, each command segment split on its own is that one command again
//!   (that the segments join back to the script holds by construction, so it is no test);
//! - a value written as a quoted string, or as a `text:` block where one can hold it, reads back
//!   as itself;
//! - an out-of-office reply read from a script, written back over it, reads back as the same
//!   reply; an unread one is written over only when named.

use engine_core::vacation::{AutoReply, VacationResponse};
use engine_sieve::{
    SegmentKind,
    dialect::{
        DialectErrorKind,
        reply::{ReplyRefusal, ScriptReply, read_reply, write_reply, write_reply_over},
    },
    segment, segment_bytes, string, tokenize,
};
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

fn reply_reads_back(script: &str) {
    let ours = VacationResponse::default()
        .with_reply(AutoReply::default().with_text("fuzz"))
        .turned_on();
    match read_reply(script) {
        ScriptReply::Read { reply, .. } => {
            // A reply read writes back, unless its window holds no whole second or the script's
            // `require` names what no `require` can be written with, and reads back as itself.
            let written = match write_reply(script, &reply) {
                Ok(written) => written,
                Err(err) => {
                    assert!(
                        matches!(
                            err.kind(),
                            DialectErrorKind::Reply(
                                ReplyRefusal::EmptyWindow | ReplyRefusal::OtherVacation
                            ) | DialectErrorKind::Require(_)
                        ),
                        "{err:?}"
                    );
                    return;
                }
            };
            let ScriptReply::Read { reply: back, .. } = read_reply(&written) else {
                panic!("a reply written reads back as a reply");
            };
            assert_eq!(back.enabled, reply.enabled);
            assert_eq!(back.window, reply.window);
            assert_eq!(back.reply, reply.reply);
        }
        ScriptReply::Unreadable(unread) => {
            // Never written over unless named; named, it is replaced in place, or refused for a
            // `${` the script would expand in what is kept of it.
            assert_eq!(
                write_reply(script, &ours).unwrap_err().kind(),
                DialectErrorKind::Reply(ReplyRefusal::Unread)
            );
            match write_reply_over(script, &ours, &unread) {
                Ok(written) => {
                    let again = read_reply(&written);
                    assert!(again.segment().is_some(), "{written:?}");
                    // Where nothing but `vacation` was kept, the reply written reads back whole.
                    if !unread.does_more() {
                        assert!(matches!(again, ScriptReply::Read { .. }), "{written:?}");
                    }
                }
                Err(err) => assert!(
                    matches!(
                        err.kind(),
                        DialectErrorKind::Reply(ReplyRefusal::Expanded | ReplyRefusal::OtherVacation)
                            | DialectErrorKind::Require(_)
                    ),
                    "{err:?}"
                ),
            }
        }
        _ => {}
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = core::str::from_utf8(data) {
        written_values_read_back(text);
        reply_reads_back(text);
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
