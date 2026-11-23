//! Lines that carry a secret, built once into a buffer of exactly their length and zeroed on drop.
//!
//! A password or token reaches the wire inside a line this crate builds: IMAP `LOGIN`, the SASL
//! blob and the `AUTHENTICATE` line carrying it, SMTP `AUTH`. Each copy is a [`Zeroizing`]
//! buffer, and each is sized up front, because a `String` that grows reallocates and frees its
//! old buffer as it was, secret included. `format!` is never used for them for that reason.

use zeroize::Zeroizing;

/// One piece of a line.
#[derive(Clone, Copy)]
pub(crate) enum Part<'a> {
    /// Written as it is.
    Text(&'a str),
    /// An IMAP quoted string (RFC 9051 §4.3): wrapped in `"`, with `\` and `"` escaped.
    Quoted(&'a str),
}

impl Part<'_> {
    fn len(self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Quoted(text) => 2 + text.len() + text.matches(['\\', '"']).count(),
        }
    }

    fn push_onto(self, line: &mut String) {
        match self {
            Self::Text(text) => line.push_str(text),
            Self::Quoted(text) => {
                line.push('"');
                for c in text.chars() {
                    if matches!(c, '\\' | '"') {
                        line.push('\\');
                    }
                    line.push(c);
                }
                line.push('"');
            }
        }
    }
}

/// `parts` joined into one buffer allocated once, at its final length.
pub(crate) fn secret_line(parts: &[Part<'_>]) -> Zeroizing<String> {
    let mut line = Zeroizing::new(String::with_capacity(
        parts.iter().map(|part| part.len()).sum(),
    ));
    for part in parts {
        part.push_onto(&mut line);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{Part, secret_line};

    #[test]
    fn a_line_is_allocated_once_at_its_final_length() {
        let line = secret_line(&[
            Part::Text("a1 LOGIN "),
            Part::Quoted("alice"),
            Part::Text(" "),
            Part::Quoted(r#"pa"ss\word"#),
            Part::Text("\r\n"),
        ]);
        assert_eq!(line.as_str(), "a1 LOGIN \"alice\" \"pa\\\"ss\\\\word\"\r\n");
        // Exactly full: a buffer that had grown would have left its old copy behind unzeroed.
        assert_eq!(line.capacity(), line.len());
    }

    #[test]
    fn quoting_matches_the_command_path() {
        for value in ["plain", r#"with "quotes""#, r"back\slash", "ünïcode"] {
            let line = secret_line(&[Part::Quoted(value)]);
            assert_eq!(line.as_str(), crate::transport_command::quote(value));
            assert_eq!(line.capacity(), line.len());
        }
    }
}
