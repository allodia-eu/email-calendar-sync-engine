use super::*;
use crate::lex::tokenize;

fn value_of(script: &str) -> Option<String> {
    value(&tokenize(script).unwrap()[0])
}

#[test]
fn a_quoted_string_reads_without_quotes_and_escapes() {
    assert_eq!(
        value_of(r#""say \"hi\" \\ there""#).as_deref(),
        Some(r#"say "hi" \ there"#)
    );
    // An undefined escape reads as the character alone (RFC 5228 §2.4.2).
    assert_eq!(value_of(r#""\a\b""#).as_deref(), Some("ab"));
    assert_eq!(
        value_of("\"two\r\nlines\"").as_deref(),
        Some("two\r\nlines")
    );
    assert_eq!(value_of("\"\"").as_deref(), Some(""));
}

#[test]
fn a_text_block_reads_its_lines_unstuffed_without_the_closing_period() {
    assert_eq!(
        value_of("text: # c\r\nfirst\r\n..second\r\n.third\r\n.\r\n").as_deref(),
        Some("first\r\n.second\r\n.third")
    );
    assert_eq!(value_of("text:\n\n.\n").as_deref(), Some(""));
    assert_eq!(value_of("text:\r\n.\r\n").as_deref(), Some(""));
    assert_eq!(value_of("text:\nx\n.").as_deref(), Some("x"));
}

#[test]
fn a_token_that_is_no_string_has_no_value() {
    assert_eq!(value_of("keep"), None);
}

#[test]
fn a_value_quotes_with_escapes_and_refuses_line_ends() {
    assert_eq!(quoted(r#"a "b" \c"#).as_deref(), Some(r#""a \"b\" \\c""#));
    assert_eq!(quoted("Größe").as_deref(), Some("\"Größe\""));
    assert_eq!(quoted("a\nb"), None);
    assert_eq!(quoted("a\rb"), None);
    assert_eq!(quoted("a\0b"), None);
}

#[test]
fn a_value_writes_as_a_text_block_with_crlf_and_doubled_periods() {
    assert_eq!(
        multi_line("Not here.\n.Go away.").as_deref(),
        Some("text:\r\nNot here.\r\n..Go away.\r\n.\r\n")
    );
    assert_eq!(
        multi_line("a\r\nb").as_deref(),
        Some("text:\r\na\r\nb\r\n.\r\n")
    );
    assert_eq!(multi_line("a\0"), None);
}

#[test]
fn every_written_value_reads_back_as_itself() {
    for original in [
        "",
        "plain",
        "q\"uo\\te",
        "é ü",
        ".dot",
        "a\r\nb",
        "..\r\n.",
        "x\r\n",
    ] {
        if let Some(text) = quoted(original) {
            assert_eq!(value_of(&text).as_deref(), Some(original));
        }
        let block = multi_line(original).unwrap();
        assert_eq!(value_of(&block).as_deref(), Some(original), "{block:?}");
    }
}
