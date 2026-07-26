//! RFC 6350 §3.4 text escaping for vCard values.
//!
//! Split out of `vcard.rs` so the codec — the part with a round-trip invariant worth
//! pinning — sits with its own tests, and to keep that file under the line limit.

/// Decodes RFC 6350 §3.4 escaping in **one left-to-right pass**.
///
/// Sequential `replace` calls cannot do this: running the `\n` rule before the `\\`
/// rule rewrites the literal text `C:\notes` (escaped as `C:\\notes`) into `C:\` +
/// newline + `otes`, and every read-modify-write round trip corrupts it further. A
/// single pass consumes each backslash together with the character it escapes, so a
/// literal backslash can never be re-read as the start of another escape.
pub(crate) fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            // The self-escaping separators decode to themselves.
            Some(escaped @ (',' | ';' | '\\')) => out.push(escaped),
            // Not a defined escape: keep the backslash and the character as-is rather
            // than dropping either.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            // Trailing lone backslash.
            None => out.push('\\'),
        }
    }
    out
}

/// Splits a value on **unescaped** occurrences of an RFC 6350 §3.4 separator (`,`
/// between list entries, `;` between structured components), unescaping each element
/// afterwards. Splitting first would turn the literal `a\,b` into two list entries
/// instead of the one value it encodes.
pub(crate) fn split_escaped_list(value: &str, separator: char) -> Vec<String> {
    split_escaped_raw(value, separator)
        .iter()
        .map(|part| unescape(part))
        .collect()
}

/// Splits like [`split_escaped_list`] but leaves each part **still escaped**, so a
/// nested split can run on it. `N` is two levels deep — `;` between components, `,`
/// within one — and unescaping between the levels would let an escaped `\,` be
/// re-read as an inner separator.
pub(crate) fn split_escaped_raw(value: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            ch if ch == separator => parts.push(std::mem::take(&mut current)),
            '\\' => {
                current.push('\\');
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            other => current.push(other),
        }
    }
    parts.push(current);
    parts
}

/// Encodes a value for an RFC 6350 §3.4 text field.
///
/// Every line-break form — `\r\n`, a lone `\r`, a lone `\n` — normalizes to the one
/// escape `\n`. A bare CR left in the output would end the content line as far as a
/// server's parser is concerned, so anything after it would be read as a *property*
/// of its own: escaping is what keeps host-supplied text from writing the wire
/// format. This mirrors `ical::format::escape_text`, which normalizes for the same
/// reason.
pub(crate) fn escape(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for ch in normalized.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{escape, split_escaped_list, unescape};

    /// The bug this codec replaced: sequential `replace` calls ran the `\n` rule
    /// before the `\\` rule, so an escaped literal backslash was re-read as the start
    /// of a newline escape. Anything `escape` produces must survive `unescape`.
    #[test]
    fn escaping_round_trips_literal_backslashes() {
        for original in [
            "C:\\notes", // the reported corruption: `\` immediately before `n`
            "back\\\\slash",
            "a\\,b",
            "trailing\\",
            "semi;colon, comma",
            "line\nbreak",
            "plain text",
            "\\n literal, not a newline",
        ] {
            assert_eq!(
                unescape(&escape(original)),
                original,
                "round trip lost data for {original:?}"
            );
            // Idempotent across repeated read-modify-write cycles, which is what
            // `patch_vcard`/`build_vcard` actually do.
            assert_eq!(unescape(&escape(&unescape(&escape(original)))), original);
        }
    }

    #[test]
    fn unescape_decodes_each_defined_sequence_once() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("a\\Nb"), "a\nb");
        assert_eq!(unescape("a\\,b"), "a,b");
        assert_eq!(unescape("a\\;b"), "a;b");
        assert_eq!(unescape("a\\\\b"), "a\\b");
        // An undefined escape keeps both characters rather than dropping either.
        assert_eq!(unescape("a\\qb"), "a\\qb");
        // A trailing lone backslash is preserved.
        assert_eq!(unescape("a\\"), "a\\");
    }

    /// `CATEGORIES` is a comma-separated list, but a comma *inside* a value is escaped.
    /// Splitting before unescaping turned one keyword into two.
    #[test]
    fn a_list_splits_only_on_unescaped_separators() {
        assert_eq!(split_escaped_list("a,b", ','), vec!["a", "b"]);
        assert_eq!(split_escaped_list("a\\,b", ','), vec!["a,b"]);
        assert_eq!(split_escaped_list("a\\,b,c", ','), vec!["a,b", "c"]);
        assert_eq!(split_escaped_list("solo", ','), vec!["solo"]);
        // A backslash-escaped backslash before a separator still separates.
        assert_eq!(split_escaped_list("a\\\\,b", ','), vec!["a\\", "b"]);
        // `N` uses `;` between components, with the same escaping rules.
        assert_eq!(
            split_escaped_list("King\\;Noel;Ada", ';'),
            vec!["King;Noel", "Ada"]
        );
    }

    /// A lone CR is not a defined escape, so `unescape` cannot round-trip one; it is
    /// normalized on the way out instead. What matters is that no raw CR or LF ever
    /// reaches the wire, where it would terminate the content line.
    #[test]
    fn escaping_normalizes_every_line_break_form() {
        assert_eq!(escape("a\r\nb"), "a\\nb");
        assert_eq!(escape("a\rb"), "a\\nb");
        assert_eq!(escape("a\nb"), "a\\nb");
        for hostile in ["x\r\nEMAIL:evil@example.test", "x\rEMAIL:evil@example.test"] {
            let escaped = escape(hostile);
            assert!(!escaped.contains('\r'));
            assert!(!escaped.contains('\n'));
        }
    }
}
