use super::*;

fn texts(script: &str) -> Vec<&str> {
    segment(script).unwrap().iter().map(Segment::text).collect()
}

fn error(script: &str) -> (SieveErrorKind, usize) {
    let err = segment(script).unwrap_err();
    (err.kind(), err.offset())
}

#[test]
fn a_rule_takes_its_name_comment_and_the_line_end_after_its_brace() {
    let script = "require [\"fileinto\"];\r\n# rule:[Invoices]\r\nif header :contains \"subject\" \"Invoice\"\r\n{\r\n\tfileinto \"Finance\";\r\n\tstop;\r\n}\r\n# rule:[Rest]\r\nif true\r\n{\r\n\tkeep;\r\n}\r\n";
    let segments = segment(script).unwrap();
    assert_eq!(
        segments.iter().map(Segment::kind).collect::<Vec<_>>(),
        vec![
            SegmentKind::Require,
            SegmentKind::Command,
            SegmentKind::Command
        ]
    );
    let rule = &segments[1];
    assert_eq!(rule.leading(), "# rule:[Invoices]\r\n");
    assert!(rule.command().starts_with("if header") && rule.command().ends_with('}'));
    assert_eq!(rule.trailer(), "\r\n");
    assert_eq!(rule.identifier(), Some("if"));
    assert_eq!(rule.start(), "require [\"fileinto\"];\r\n".len());
    assert_eq!(rule.end(), segments[2].start());
    assert_eq!(segments[2].leading(), "# rule:[Rest]\r\n");
    assert_eq!(texts(script).concat(), script);
}

#[test]
fn an_if_chain_is_one_segment() {
    let script = "if true { keep; }\r\nelsif false { discard; }\r\n  ELSE { stop; }\r\nkeep;";
    assert_eq!(
        texts(script),
        vec![
            "if true { keep; }\r\nelsif false { discard; }\r\n  ELSE { stop; }\r\n",
            "keep;"
        ]
    );
}

#[test]
fn a_command_keeps_the_comment_on_its_line_and_the_next_line_leads_the_next() {
    assert_eq!(
        texts("keep; # why\r\n\r\n/* about stop */ stop;  \r\n  "),
        vec!["keep; # why\r\n", "\r\n/* about stop */ stop;  \r\n", "  "]
    );
    assert_eq!(texts("keep;stop;"), vec!["keep;", "stop;"]);
    assert_eq!(texts("keep; stop;"), vec!["keep;", " stop;"]);
    assert_eq!(texts("keep; # end"), vec!["keep; # end"]);
}

#[test]
fn semicolons_and_braces_inside_strings_comments_and_brackets_do_not_split() {
    let script = "if anyof (header :is \"x\" \"};\", /* ; } */ true) { reject text:\r\n};\r\n.\r\n; }\r\nkeep;";
    let segments = texts(script);
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[1], "keep;");
}

#[test]
fn a_script_with_no_command_is_one_trailing_segment() {
    let segments = segment("# only a comment\r\n").unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].kind(), SegmentKind::Trailing);
    assert_eq!(segments[0].identifier(), None);
    assert_eq!(segments[0].tokens(), &[]);
    assert_eq!(segments[0].leading(), "# only a comment\r\n");
    assert_eq!(segments[0].command(), "");
    assert_eq!(segment("").unwrap(), Vec::new());
}

#[test]
fn a_script_that_cannot_be_split_is_refused_where_it_breaks() {
    assert_eq!(error("keep; ;"), (SieveErrorKind::ExpectedCommand, 6));
    assert_eq!(error("\"x\";"), (SieveErrorKind::ExpectedCommand, 0));
    assert_eq!(
        error("if true { keep; ]"),
        (SieveErrorKind::UnbalancedBracket, 16)
    );
    assert_eq!(
        error("if true { keep; }}"),
        (SieveErrorKind::ExpectedCommand, 17)
    );
    assert_eq!(error("x )"), (SieveErrorKind::UnbalancedBracket, 2));
    assert_eq!(
        error("keep; if true { keep;"),
        (SieveErrorKind::UnterminatedCommand, 6)
    );
    assert_eq!(error("keep"), (SieveErrorKind::UnterminatedCommand, 0));
    assert_eq!(error("keep; @"), (SieveErrorKind::UnexpectedCharacter, 6));
    assert_eq!(
        error("\u{feff}keep;"),
        (SieveErrorKind::UnexpectedCharacter, 0),
        "a byte order mark is no token"
    );
}

#[test]
fn bytes_must_be_utf8() {
    let err = segment_bytes(b"keep; \xff").unwrap_err();
    assert_eq!((err.kind(), err.offset()), (SieveErrorKind::NotUtf8, 6));
    assert_eq!(segment_bytes(b"keep;").unwrap().len(), 1);
    assert!(err.to_string().contains("byte 6"));
}

#[test]
fn debug_shows_the_span_and_never_the_text() {
    let segments = segment("# rule:[secret]\r\nkeep;").unwrap();
    let debug = format!("{segments:?}");
    assert!(!debug.contains("secret"), "{debug}");
    assert!(debug.contains("Command"), "{debug}");
}
