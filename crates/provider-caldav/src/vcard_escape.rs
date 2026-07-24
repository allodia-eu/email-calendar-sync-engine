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

/// Splits a value on **unescaped** commas (RFC 6350 §3.4 list separator), unescaping
/// each element afterwards. Splitting first would turn the literal `a\,b` into two
/// list entries instead of the one value it encodes.
pub(crate) fn split_escaped_list(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            ',' => parts.push(std::mem::take(&mut current)),
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
    parts.iter().map(|part| unescape(part)).collect()
}

pub(crate) fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(',', "\\,")
        .replace(';', "\\;")
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
    fn a_list_splits_only_on_unescaped_commas() {
        assert_eq!(split_escaped_list("a,b"), vec!["a", "b"]);
        assert_eq!(split_escaped_list("a\\,b"), vec!["a,b"]);
        assert_eq!(split_escaped_list("a\\,b,c"), vec!["a,b", "c"]);
        assert_eq!(split_escaped_list("solo"), vec!["solo"]);
        // A backslash-escaped backslash before a separator still separates.
        assert_eq!(split_escaped_list("a\\\\,b"), vec!["a\\", "b"]);
    }
}
