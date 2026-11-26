//! The `Content-ID` an inline draft part is referenced by.

use core::fmt;

use serde::{Deserialize, Serialize};

/// A validated `Content-ID` header value without surrounding angle brackets.
///
/// The type keeps inline-image references distinct from arbitrary strings and
/// prevents the characters that would break the header boundary.
///
/// A generated `Content-ID` commonly ends in the sender's domain, so `Debug` shows the value
/// only where [`engine_core::redact::SHOWS_VALUES`] holds; `Display` and
/// [`as_str`](Self::as_str) always do.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentIdHeader(Box<str>);

impl fmt::Debug for ContentIdHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ContentIdHeader")
            .field(&engine_core::redact::Content(&self.0))
            .finish()
    }
}

impl ContentIdHeader {
    /// Builds a content id value without surrounding angle brackets.
    ///
    /// # Errors
    ///
    /// Returns [`ContentIdError`] when the value is empty or contains a character
    /// that cannot be safely placed inside a `Content-ID` header.
    pub fn new(value: impl Into<String>) -> Result<Self, ContentIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ContentIdError::Empty);
        }
        if value
            .bytes()
            .any(|b| matches!(b, b'\r' | b'\n' | b'\0' | b'<' | b'>'))
        {
            return Err(ContentIdError::ForbiddenCharacter);
        }
        Ok(Self(value.into_boxed_str()))
    }

    /// Returns the header value without surrounding angle brackets.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContentIdHeader {
    type Error = ContentIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ContentIdHeader> for String {
    fn from(value: ContentIdHeader) -> Self {
        value.0.into()
    }
}

impl fmt::Display for ContentIdHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a `Content-ID` header value was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentIdError {
    /// The value was empty.
    Empty,
    /// The value contained CR, LF, NUL, `<`, or `>`.
    ForbiddenCharacter,
}

impl fmt::Display for ContentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("content id must not be empty"),
            Self::ForbiddenCharacter => {
                f.write_str("content id contains a forbidden character (CR, LF, NUL, '<', or '>')")
            }
        }
    }
}

impl std::error::Error for ContentIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_a_content_id_only_where_the_build_shows_values() {
        // A generated `Content-ID` commonly ends in the sender's domain.
        let id = ContentIdHeader::new("logo.1@mail.example.com").unwrap();
        let expected = if engine_core::redact::SHOWS_VALUES {
            r#"ContentIdHeader("logo.1@mail.example.com")"#
        } else {
            "ContentIdHeader(<redacted, 23 bytes>)"
        };
        assert_eq!(format!("{id:?}"), expected);
    }
}
