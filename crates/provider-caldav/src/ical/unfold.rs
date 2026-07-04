//! iCalendar content-line unfolding and tokenizing (RFC 5545 §3.1, §3.2).
//!
//! A logical content line is `NAME[;PARAM=value[;…]]:VALUE`. Lines are folded by
//! inserting CRLF + a single space/tab; unfolding reverses that. The split into
//! name/params/value is quote-aware: a `:` or `;` inside a `DQUOTE`-quoted
//! parameter value (e.g. a URI) is not a delimiter. Malformed lines (no colon)
//! are skipped rather than aborting the parse, so a hostile resource cannot stop
//! the rest of the calendar from being read.

/// A single unfolded content line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContentLine {
    /// The property name, uppercased (names are case-insensitive, RFC 5545 §3.2).
    pub name: String,
    /// Parameters as `(uppercased name, unquoted value)` pairs, in source order.
    pub params: Vec<(String, String)>,
    /// The raw value, still TEXT-escaped (use [`unescape_text`] for TEXT values).
    pub value: String,
}

impl ContentLine {
    /// The first value of the parameter named `name` (case-insensitive), if any.
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Unfolds `text` and tokenizes every well-formed line into a [`ContentLine`].
pub(crate) fn content_lines(text: &str) -> Vec<ContentLine> {
    unfold(text)
        .iter()
        .filter_map(|line| tokenize(line))
        .collect()
}

/// Joins folded continuation lines (those beginning with a space or tab) onto
/// their predecessor, tolerating both CRLF and bare-LF endings.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
            continue;
        }
        lines.push(line.to_owned());
    }
    lines
}

/// Tokenizes one unfolded line, or `None` if it has no `name:value` shape.
fn tokenize(line: &str) -> Option<ContentLine> {
    let (head, value) = split_once_unquoted(line, ':')?;
    let mut segments = split_unquoted(head, ';').into_iter();
    let name = segments.next()?.to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = segments.filter_map(parse_param).collect();
    Some(ContentLine {
        name,
        params,
        value: value.to_owned(),
    })
}

/// Splits at the first `delim` that is not inside a `DQUOTE`-quoted run.
pub(crate) fn split_once_unquoted(text: &str, delim: char) -> Option<(&str, &str)> {
    let mut in_quote = false;
    for (i, c) in text.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            _ if c == delim && !in_quote => return Some((&text[..i], &text[i + c.len_utf8()..])),
            _ => {}
        }
    }
    None
}

/// Splits `text` on every `delim` outside a quoted run.
pub(crate) fn split_unquoted(text: &str, delim: char) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            _ if c == delim && !in_quote => {
                segments.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    segments.push(&text[start..]);
    segments
}

/// Parses one `KEY=value` (or `KEY="value"`) parameter, uppercasing the name and
/// stripping surrounding quotes. A segment with no `=` is dropped.
fn parse_param(segment: &str) -> Option<(String, String)> {
    let (key, value) = segment.split_once('=')?;
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    Some((key.trim().to_ascii_uppercase(), value.to_owned()))
}

/// Unescapes an iCalendar TEXT value (RFC 5545 §3.3.11): `\\`, `\;`, `\,`, and
/// `\n`/`\N` (a newline). An unknown escape keeps the escaped character verbatim.
pub(crate) fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfolds_crlf_and_lf_continuations() {
        let text = "DESCRIPTION:line one\r\n  still line one\nSUMMARY:hi\r\n";
        let lines = content_lines(text);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].name, "DESCRIPTION");
        // The leading single space of the continuation is consumed; the rest joins.
        assert_eq!(lines[0].value, "line one still line one");
        assert_eq!(lines[1].name, "SUMMARY");
    }

    #[test]
    fn parses_params_and_uppercases_names() {
        let lines =
            content_lines("dtstart;tzid=Europe/Amsterdam;VALUE=DATE-TIME:20260318T100000\n");
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line.name, "DTSTART");
        assert_eq!(line.param("TZID"), Some("Europe/Amsterdam"));
        assert_eq!(line.param("value"), Some("DATE-TIME")); // case-insensitive lookup
        assert_eq!(line.value, "20260318T100000");
    }

    #[test]
    fn colon_inside_a_quoted_param_is_not_the_value_separator() {
        // A CONFERENCE URI value plus a quoted LABEL param containing a colon.
        let lines = content_lines(
            "CONFERENCE;VALUE=URI;LABEL=\"Join: now\":https://meet.example.com/room\n",
        );
        let line = &lines[0];
        assert_eq!(line.name, "CONFERENCE");
        assert_eq!(line.param("LABEL"), Some("Join: now"));
        assert_eq!(line.value, "https://meet.example.com/room");
    }

    #[test]
    fn unescapes_text_values() {
        assert_eq!(unescape_text(r"a\, b\; c\nd\\e"), "a, b; c\nd\\e");
        // A trailing lone backslash is preserved rather than panicking.
        assert_eq!(unescape_text(r"x\"), "x\\");
    }

    #[test]
    fn malformed_lines_without_a_colon_are_skipped() {
        let lines = content_lines("VERSION:2.0\nGARBAGE WITHOUT COLON\nSUMMARY:ok\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].name, "VERSION");
        assert_eq!(lines[1].name, "SUMMARY");
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(content_lines("").is_empty());
        assert!(content_lines("\r\n\r\n").is_empty());
    }
}
