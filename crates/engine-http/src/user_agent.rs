//! The product every request names in its `User-Agent`: the host's, never the engine's.
//!
//! RFC 9110 §10.1.5 says a client SHOULD send `User-Agent`, and Yahoo's address-book server
//! fails a valid request without one (`docs/spec-deviations.md`, YAHOO-6). The value names the
//! application a person installed, so a provider can tell its traffic apart and know whom to
//! contact about it; a host supplies it once and hands it to every provider it connects, the
//! way it hands out its TLS and throttling policies.

use std::fmt;

/// A `User-Agent` product token, `name/version` (RFC 9110 §10.1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAgent(String);

/// A product name or version that is not an RFC 9110 token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUserAgent(String);

impl fmt::Display for InvalidUserAgent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} is not an HTTP token (RFC 9110 §5.6.2)",
            self.0
        )
    }
}

impl std::error::Error for InvalidUserAgent {}

impl UserAgent {
    /// The product `name` at `version`, sent as `name/version`.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidUserAgent`] when either part is empty or holds a character an HTTP
    /// token may not (a space, a `/`, a quote, a control character, or any non-ASCII).
    pub fn new(name: &str, version: &str) -> Result<Self, InvalidUserAgent> {
        for part in [name, version] {
            if part.is_empty() || !part.bytes().all(is_tchar) {
                return Err(InvalidUserAgent(part.to_owned()));
            }
        }
        Ok(Self(format!("{name}/{version}")))
    }

    /// The header value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserAgent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// RFC 9110 §5.6.2 `tchar`.
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_product_is_its_name_and_version() {
        let agent = UserAgent::new("example-mail", "1.2.3").unwrap();
        assert_eq!(agent.as_str(), "example-mail/1.2.3");
    }

    #[test]
    fn a_part_that_is_not_a_token_is_refused() {
        for (name, version) in [
            ("", "1"),
            ("example", ""),
            ("example mail", "1"),
            ("example/mail", "1"),
            ("example", "1 (beta)"),
            ("exämple", "1"),
        ] {
            assert!(
                UserAgent::new(name, version).is_err(),
                "{name:?} {version:?}"
            );
        }
    }
}
