use super::*;

fn kinds(script: &str) -> Vec<(TokenKind, &str)> {
    tokenize(script)
        .unwrap()
        .into_iter()
        .map(|token| (token.kind(), token.text()))
        .collect()
}

fn error(script: &str) -> (SieveErrorKind, usize) {
    let err = tokenize(script).unwrap_err();
    (err.kind(), err.offset())
}

#[test]
fn every_token_kind_spans_its_bytes() {
    use TokenKind::*;
    assert_eq!(
        kinds("if size :over 10K { fileinto [\"a\", \"b\"]; } # done\r\n/* c */"),
        vec![
            (Identifier, "if"),
            (Whitespace, " "),
            (Identifier, "size"),
            (Whitespace, " "),
            (Tag, ":over"),
            (Whitespace, " "),
            (Number, "10K"),
            (Whitespace, " "),
            (LeftBrace, "{"),
            (Whitespace, " "),
            (Identifier, "fileinto"),
            (Whitespace, " "),
            (LeftBracket, "["),
            (QuotedString, "\"a\""),
            (Comma, ","),
            (Whitespace, " "),
            (QuotedString, "\"b\""),
            (RightBracket, "]"),
            (Semicolon, ";"),
            (Whitespace, " "),
            (RightBrace, "}"),
            (Whitespace, " "),
            (HashComment, "# done"),
            (Whitespace, "\r\n"),
            (BracketComment, "/* c */"),
        ]
    );
    assert_eq!(
        kinds("anyof(a)"),
        vec![
            (Identifier, "anyof"),
            (LeftParen, "("),
            (Identifier, "a"),
            (RightParen, ")")
        ]
    );
}

#[test]
fn a_quoted_string_holds_escapes_brackets_comment_marks_and_line_ends() {
    let script = "\"a \\\" } ; # /* \\\\\r\nb\"";
    assert_eq!(kinds(script), vec![(TokenKind::QuotedString, script)]);
}

#[test]
fn a_text_block_runs_to_the_line_holding_only_a_period() {
    let block = "text: # why\r\nline } ;\r\n..dotted\r\n.not the end\r\n.\r\n";
    assert_eq!(
        kinds(&format!("{block};")),
        vec![(TokenKind::MultiLine, block), (TokenKind::Semicolon, ";")]
    );
    // LF line ends, and a closing period at the end of the script.
    assert_eq!(
        kinds("TEXT:\nx\n."),
        vec![(TokenKind::MultiLine, "TEXT:\nx\n.")]
    );
}

#[test]
fn text_without_its_colon_is_an_identifier() {
    assert_eq!(
        kinds("text :x"),
        vec![
            (TokenKind::Identifier, "text"),
            (TokenKind::Whitespace, " "),
            (TokenKind::Tag, ":x")
        ]
    );
}

#[test]
fn a_hash_comment_stops_before_its_line_end() {
    assert_eq!(
        kinds("#a\r\n#b\n#c\r"),
        vec![
            (TokenKind::HashComment, "#a"),
            (TokenKind::Whitespace, "\r\n"),
            (TokenKind::HashComment, "#b"),
            (TokenKind::Whitespace, "\n"),
            (TokenKind::HashComment, "#c\r"),
        ]
    );
}

#[test]
fn numbers_take_one_quantifier_in_either_case() {
    assert_eq!(
        kinds("1m 2G 3k 4"),
        vec![
            (TokenKind::Number, "1m"),
            (TokenKind::Whitespace, " "),
            (TokenKind::Number, "2G"),
            (TokenKind::Whitespace, " "),
            (TokenKind::Number, "3k"),
            (TokenKind::Whitespace, " "),
            (TokenKind::Number, "4"),
        ]
    );
}

#[test]
fn unterminated_strings_and_comments_are_errors_at_their_start() {
    assert_eq!(
        error("keep; \"open"),
        (SieveErrorKind::UnterminatedString, 6)
    );
    assert_eq!(
        error("x \"ends in a backslash\\"),
        (SieveErrorKind::UnterminatedString, 2)
    );
    assert_eq!(
        error("x text:\r\nno end\r\n"),
        (SieveErrorKind::UnterminatedMultiLine, 2)
    );
    assert_eq!(error("x text:"), (SieveErrorKind::UnterminatedMultiLine, 2));
    // Anything but white space or a comment after `text:` is no token at all.
    assert_eq!(
        error("x text: junk\r\n.\r\n"),
        (SieveErrorKind::UnexpectedCharacter, 8)
    );
    assert_eq!(error("x /* open"), (SieveErrorKind::UnterminatedComment, 2));
}

#[test]
fn a_byte_that_starts_no_token_is_an_error() {
    assert_eq!(error("keep; @"), (SieveErrorKind::UnexpectedCharacter, 6));
    assert_eq!(error("a / b"), (SieveErrorKind::UnexpectedCharacter, 2));
    assert_eq!(error(": x"), (SieveErrorKind::UnexpectedCharacter, 0));
    assert_eq!(error("é"), (SieveErrorKind::UnexpectedCharacter, 0));
}

#[test]
fn identifiers_and_tags_compare_ignoring_case() {
    let tokens = tokenize("IF :Contains").unwrap();
    assert!(tokens[0].is_identifier("if"));
    assert!(!tokens[0].is_tag("if"));
    assert!(tokens[2].is_tag("contains"));
    assert!(!tokens[2].is_identifier("contains"));
    assert_eq!((tokens[2].start(), tokens[2].end()), (3, 12));
}

#[test]
fn debug_shows_the_span_and_never_the_text() {
    let tokens = tokenize("\"secret\"").unwrap();
    let debug = format!("{:?}", tokens[0]);
    assert!(!debug.contains("secret"), "{debug}");
    assert!(
        debug.contains("QuotedString") && debug.contains("len: 8"),
        "{debug}"
    );
    assert!(!TokenKind::QuotedString.is_trivia() && TokenKind::MultiLine.is_string());
}
