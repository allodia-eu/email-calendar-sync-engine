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
fn a_text_block_keeps_the_line_end_before_its_closing_period() {
    // RFC 5228 §2.4.2: "The CRLF before the final period is considered part of the value."
    assert_eq!(
        value_of("text:\r\nhello\r\n.\r\n").as_deref(),
        Some("hello\r\n")
    );
    assert_eq!(
        value_of("text: # c\r\nfirst\r\n..second\r\n.third\r\n.\r\n").as_deref(),
        Some("first\r\n.second\r\n.third\r\n")
    );
    assert_eq!(value_of("text:\n\n.\n").as_deref(), Some("\n"));
    assert_eq!(value_of("text:\r\n.\r\n").as_deref(), Some(""));
    assert_eq!(value_of("text:\nx\n.").as_deref(), Some("x\n"));
}

#[test]
fn a_token_that_is_no_string_has_no_value() {
    assert_eq!(value_of("keep"), None);
}

#[test]
fn a_value_quotes_with_escapes_and_refuses_a_bare_line_break_or_nul() {
    assert_eq!(quoted(r#"a "b" \c"#).as_deref(), Some(r#""a \"b\" \\c""#));
    assert_eq!(quoted("Größe").as_deref(), Some("\"Größe\""));
    assert_eq!(quoted("a\r\nb").as_deref(), Some("\"a\r\nb\""));
    for bare in ["a\nb", "a\rb", "a\r", "\nb", "a\0b"] {
        assert_eq!(quoted(bare), None, "{bare:?}");
    }
}

#[test]
fn a_value_writes_as_a_text_block_only_where_one_reads_back_as_it() {
    assert_eq!(
        multi_line("Not here.\r\n.Go away.\r\n").as_deref(),
        Some("text:\r\nNot here.\r\n..Go away.\r\n.\r\n")
    );
    assert_eq!(multi_line("").as_deref(), Some("text:\r\n.\r\n"));
    // No final CRLF, a bare CR or LF, a NUL: no block reads back as that.
    for unwritable in ["a", "a\r\nb", "a\rb\r\n", "a\nb\r\n", "a\r", "a\0\r\n"] {
        assert_eq!(multi_line(unwritable), None, "{unwritable:?}");
    }
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
        ".\r\n",
        "\r\n",
    ] {
        let text = quoted(original).unwrap();
        assert_eq!(value_of(&text).as_deref(), Some(original), "{text:?}");
        if let Some(block) = multi_line(original) {
            assert_eq!(value_of(&block).as_deref(), Some(original), "{block:?}");
        }
    }
}
