//! Message keywords.
//!
//! Keywords are the user-settable state axis (read, flagged, …), distinct from
//! mailbox membership (collection placement) and from a mailbox's normalized
//! role. JMAP keywords and IMAP flags map to this one type; some Gmail system
//! labels (`UNREAD`→absence of `$seen`, `STARRED`→`$flagged`) are keywords too,
//! not membership.
//!
//! A keyword is case-insensitive and stored lowercased, 1–255 characters from
//! ASCII `%x21-%x7e`, excluding `( ) { ] % * " \` (RFC 8621 §4.1.1, RFC 5788).
//! The IMAP `\Recent` and `\Deleted` flags are deliberately **not** keywords:
//! `\Recent` is a session flag and `\Deleted` belongs to IMAP's expunge model.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// Error returned when constructing a [`Keyword`] fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeywordError {
    /// The keyword was empty.
    #[error("a keyword must not be empty")]
    Empty,
    /// The keyword exceeded 255 characters.
    #[error("a keyword must be at most 255 characters, found {actual}")]
    TooLong {
        /// The actual length, in characters.
        actual: usize,
    },
    /// The keyword contained a control character, space, non-ASCII byte, or one
    /// of the forbidden characters `( ) { ] % * " \`.
    #[error("a keyword must not contain {found:?}")]
    InvalidCharacter {
        /// The offending character.
        found: char,
    },
}

/// A message keyword, stored in canonical lowercase form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Keyword(Box<str>);

impl Keyword {
    /// Maximum length of a keyword, in characters.
    pub const MAX_LEN: usize = 255;

    /// Creates a keyword, validating its characters and lowercasing it.
    ///
    /// # Errors
    ///
    /// Returns [`KeywordError`] if the value is empty, longer than
    /// [`Self::MAX_LEN`], or contains a forbidden character.
    pub fn new(value: impl Into<String>) -> Result<Self, KeywordError> {
        let mut value = value.into();
        if value.is_empty() {
            return Err(KeywordError::Empty);
        }
        if value.len() > Self::MAX_LEN {
            return Err(KeywordError::TooLong {
                actual: value.chars().count(),
            });
        }
        for &byte in value.as_bytes() {
            let forbidden = matches!(byte, b'(' | b')' | b'{' | b']' | b'%' | b'*' | b'"' | b'\\');
            if !(0x21..=0x7e).contains(&byte) || forbidden {
                return Err(KeywordError::InvalidCharacter {
                    found: byte as char,
                });
            }
        }
        value.make_ascii_lowercase();
        Ok(Self(value.into_boxed_str()))
    }

    /// Returns the canonical keyword for a [`SystemKeyword`].
    #[must_use]
    pub fn system(keyword: SystemKeyword) -> Self {
        Self(keyword.as_str().into())
    }

    /// Returns the corresponding [`SystemKeyword`] if this is a well-known one.
    #[must_use]
    pub fn as_system(&self) -> Option<SystemKeyword> {
        SystemKeyword::from_canonical(&self.0)
    }

    /// Returns the keyword as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Keyword {
    type Err = KeywordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for Keyword {
    type Error = KeywordError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Keyword> for String {
    fn from(value: Keyword) -> Self {
        value.0.into()
    }
}

/// A keyword with standardized cross-provider meaning (RFC 5788 / RFC 8621
/// §4.1.1). The four "special" keywords (`$draft`, `$seen`, `$flagged`,
/// `$answered`) drive client behavior; the rest are registered conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SystemKeyword {
    /// `$draft` — the message is a draft being composed.
    Draft,
    /// `$seen` — the message has been read.
    Seen,
    /// `$flagged` — flagged for urgent/special attention (Gmail `STARRED`).
    Flagged,
    /// `$answered` — the message has been replied to.
    Answered,
    /// `$forwarded` — the message has been forwarded.
    Forwarded,
    /// `$junk` — classified as junk/spam.
    Junk,
    /// `$notjunk` — classified as not junk.
    NotJunk,
    /// `$phishing` — flagged as a phishing attempt.
    Phishing,
    /// `$mdnsent` — a message disposition notification has been sent.
    MdnSent,
}

impl SystemKeyword {
    /// Returns the canonical `$`-prefixed lowercase spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "$draft",
            Self::Seen => "$seen",
            Self::Flagged => "$flagged",
            Self::Answered => "$answered",
            Self::Forwarded => "$forwarded",
            Self::Junk => "$junk",
            Self::NotJunk => "$notjunk",
            Self::Phishing => "$phishing",
            Self::MdnSent => "$mdnsent",
        }
    }

    /// Returns the system keyword for a canonical (already lowercased) string.
    fn from_canonical(value: &str) -> Option<Self> {
        Some(match value {
            "$draft" => Self::Draft,
            "$seen" => Self::Seen,
            "$flagged" => Self::Flagged,
            "$answered" => Self::Answered,
            "$forwarded" => Self::Forwarded,
            "$junk" => Self::Junk,
            "$notjunk" => Self::NotJunk,
            "$phishing" => Self::Phishing,
            "$mdnsent" => Self::MdnSent,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_is_lowercased() {
        let kw = Keyword::new("MyLabel").unwrap();
        assert_eq!(kw.as_str(), "mylabel");
        // Case-insensitive: differing case collapses to equality.
        assert_eq!(Keyword::new("FOO").unwrap(), Keyword::new("foo").unwrap());
    }

    #[test]
    fn keyword_rejects_invalid_characters() {
        assert_eq!(Keyword::new(""), Err(KeywordError::Empty));
        for bad in [
            "a b", "a(b", "a)b", "a{b", "a]b", "a%b", "a*b", "a\"b", "a\\b",
        ] {
            let offending = bad.as_bytes()[1] as char;
            assert_eq!(
                Keyword::new(bad),
                Err(KeywordError::InvalidCharacter { found: offending }),
                "should reject {bad:?}"
            );
        }
        // A non-ASCII character is rejected too.
        assert!(Keyword::new("é").is_err());
    }

    #[test]
    fn keyword_length_bound() {
        assert!(Keyword::new("a".repeat(255)).is_ok());
        assert!(matches!(
            Keyword::new("a".repeat(256)),
            Err(KeywordError::TooLong { actual: 256 })
        ));
    }

    #[test]
    fn system_keywords_roundtrip() {
        for sk in [
            SystemKeyword::Draft,
            SystemKeyword::Seen,
            SystemKeyword::Flagged,
            SystemKeyword::Answered,
            SystemKeyword::Forwarded,
            SystemKeyword::Junk,
            SystemKeyword::NotJunk,
            SystemKeyword::Phishing,
            SystemKeyword::MdnSent,
        ] {
            let kw = Keyword::system(sk);
            assert_eq!(kw.as_system(), Some(sk));
            assert!(kw.as_str().starts_with('$'));
        }
        // A user keyword is not a system keyword.
        assert!(Keyword::new("project-x").unwrap().as_system().is_none());
        // Case-insensitive system keyword recognition.
        assert_eq!(
            Keyword::new("$SEEN").unwrap().as_system(),
            Some(SystemKeyword::Seen)
        );
    }

    #[test]
    fn deserialization_validates() {
        let kw: Keyword = serde_json::from_str("\"$flagged\"").unwrap();
        assert_eq!(kw.as_system(), Some(SystemKeyword::Flagged));
        assert!(serde_json::from_str::<Keyword>("\"bad keyword\"").is_err());
    }
}
