//! The address a conference is joined at.

use serde::{Deserialize, Serialize};

/// The URI schemes a conference may be joined through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UriScheme {
    /// `https:`, a web join link.
    Https,
    /// `http:`, a web join link without transport security.
    Http,
    /// `tel:` (RFC 3966), a dial-in number.
    Tel,
    /// `sip:` (RFC 3261), a SIP address.
    Sip,
    /// `sips:` (RFC 3261), a SIP address over TLS.
    Sips,
}

impl UriScheme {
    fn parse(scheme: &str) -> Option<Self> {
        Some(match scheme.to_ascii_lowercase().as_str() {
            "https" => Self::Https,
            "http" => Self::Http,
            "tel" => Self::Tel,
            "sip" => Self::Sip,
            "sips" => Self::Sips,
            _ => return None,
        })
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
            Self::Tel => "tel",
            Self::Sip => "sip",
            Self::Sips => "sips",
        }
    }

    /// Whether the scheme names a web page (`https:` or `http:`).
    #[must_use]
    pub const fn is_web(self) -> bool {
        matches!(self, Self::Https | Self::Http)
    }
}

/// Why a value is not a [`ConferenceUri`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConferenceUriError {
    /// The value has no `scheme:` prefix, or nothing after it.
    #[error("not a URI")]
    NotAUri,
    /// The scheme is not one a conference is joined through.
    #[error("unsupported URI scheme")]
    UnsupportedScheme,
    /// A web URI without a host, or with credentials before it.
    #[error("not a usable web address")]
    BadAuthority,
    /// The value holds whitespace, a control character or a backslash.
    #[error("URI holds whitespace, a control character or a backslash")]
    BadCharacter,
}

/// A URI a person can join a conference at: `https`, `http`, `tel`, `sip` or `sips`.
///
/// Any other scheme is refused, because a host opens this value when the person asks to join: a
/// `javascript:`, `file:` or `data:` URI reaching that action is an attack, not a meeting. A web
/// URI must name a host and carry no credentials, since `https://meet.example.com@evil.example/`
/// reads as one host and opens another. The scheme is lowercased; the rest is kept as the server
/// wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ConferenceUri(String);

impl ConferenceUri {
    /// Parses `value`, trimming surrounding whitespace.
    ///
    /// A `tel:` number written with spaces, as dial-in numbers commonly are, loses them: RFC 3966
    /// allows none, and the digits are what dials. A backslash is refused everywhere, because a
    /// browser reads it as `/` and would open a different host from the one [`Self::host`] names.
    ///
    /// # Errors
    ///
    /// Returns a [`ConferenceUriError`] when `value` is not a URI of a supported scheme.
    pub fn parse(value: &str) -> Result<Self, ConferenceUriError> {
        let value = value.trim();
        let (scheme, rest) = value.split_once(':').ok_or(ConferenceUriError::NotAUri)?;
        let scheme = UriScheme::parse(scheme).ok_or(ConferenceUriError::UnsupportedScheme)?;
        let rest = if scheme == UriScheme::Tel {
            rest.replace(' ', "")
        } else {
            rest.to_owned()
        };
        if rest
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        {
            return Err(ConferenceUriError::BadCharacter);
        }
        if rest.is_empty() {
            return Err(ConferenceUriError::NotAUri);
        }
        let uri = Self(format!("{}:{rest}", scheme.as_str()));
        if scheme.is_web() {
            let (authority, _) = uri.web_parts().ok_or(ConferenceUriError::BadAuthority)?;
            if authority.is_empty() || authority.contains('@') {
                return Err(ConferenceUriError::BadAuthority);
            }
        }
        Ok(uri)
    }

    /// The URI as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The scheme.
    #[must_use]
    pub fn scheme(&self) -> UriScheme {
        let scheme = self.0.split_once(':').map_or("", |(scheme, _)| scheme);
        UriScheme::parse(scheme).unwrap_or_else(|| unreachable!("checked when parsed"))
    }

    /// The host of a web URI, lowercased, without its port or a trailing dot; `None` for any
    /// other scheme.
    #[must_use]
    pub fn host(&self) -> Option<String> {
        let (authority, _) = self.web_parts()?;
        let host = match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => authority,
        };
        Some(host.trim_end_matches('.').to_ascii_lowercase())
    }

    /// The path of a web URI, without its query or fragment (`""` when there is none); `None`
    /// for any other scheme.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.web_parts().map(|(_, path)| path)
    }

    /// A web URI's authority and path, or `None` when it has no `//` or is not a web URI.
    fn web_parts(&self) -> Option<(&str, &str)> {
        let rest = self
            .0
            .strip_prefix("https://")
            .or_else(|| self.0.strip_prefix("http://"))?;
        let end = rest.find(['?', '#']).unwrap_or(rest.len());
        let rest = &rest[..end];
        Some(match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, ""),
        })
    }
}

impl TryFrom<String> for ConferenceUri {
    type Error = ConferenceUriError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ConferenceUri> for String {
    fn from(uri: ConferenceUri) -> Self {
        uri.0
    }
}

impl std::fmt::Display for ConferenceUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(value: &str) -> ConferenceUri {
        ConferenceUri::parse(value).unwrap()
    }

    #[test]
    fn accepts_the_join_schemes_and_lowercases_the_scheme() {
        assert_eq!(
            uri("HTTPS://Meet.Example.com/Room").as_str(),
            "https://Meet.Example.com/Room"
        );
        assert_eq!(uri(" tel:+31201234567 ").scheme(), UriScheme::Tel);
        assert_eq!(uri("sip:room@example.com").scheme(), UriScheme::Sip);
        assert_eq!(uri("sips:room@example.com").scheme(), UriScheme::Sips);
        assert_eq!(uri("http://meet.example.com/x").scheme(), UriScheme::Http);
    }

    #[test]
    fn refuses_a_scheme_a_host_must_never_open() {
        for value in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,x",
            "mailto:a@example.com",
        ] {
            assert_eq!(
                ConferenceUri::parse(value),
                Err(ConferenceUriError::UnsupportedScheme),
                "{value}"
            );
        }
    }

    #[test]
    fn a_dial_in_number_loses_its_spaces() {
        assert_eq!(uri("tel:+31 20 123 4567").as_str(), "tel:+31201234567");
        assert_eq!(
            ConferenceUri::parse("sip:a b@example.com"),
            Err(ConferenceUriError::BadCharacter)
        );
    }

    #[test]
    fn refuses_a_backslash_a_browser_would_read_as_a_slash() {
        assert_eq!(
            ConferenceUri::parse("https://evil.example\\.zoom.us/j/1"),
            Err(ConferenceUriError::BadCharacter)
        );
    }

    #[test]
    fn refuses_what_is_not_a_uri() {
        assert_eq!(
            ConferenceUri::parse("meet.example.com/x"),
            Err(ConferenceUriError::NotAUri)
        );
        assert_eq!(
            ConferenceUri::parse("tel:"),
            Err(ConferenceUriError::NotAUri)
        );
        assert_eq!(
            ConferenceUri::parse("https://meet.example.com/a b"),
            Err(ConferenceUriError::BadCharacter)
        );
    }

    #[test]
    fn refuses_a_web_uri_without_a_host_or_with_credentials() {
        assert_eq!(
            ConferenceUri::parse("https:meet"),
            Err(ConferenceUriError::BadAuthority)
        );
        assert_eq!(
            ConferenceUri::parse("https:///room"),
            Err(ConferenceUriError::BadAuthority)
        );
        assert_eq!(
            ConferenceUri::parse("https://meet.example.com@evil.example/room"),
            Err(ConferenceUriError::BadAuthority)
        );
    }

    #[test]
    fn splits_a_web_uri_into_host_and_path() {
        let join = uri("https://Team.Example.COM.:8443/j/123?pwd=x#top");
        assert_eq!(join.host().as_deref(), Some("team.example.com"));
        assert_eq!(join.path(), Some("/j/123"));
        assert_eq!(uri("https://example.com").path(), Some(""));
        assert_eq!(uri("tel:+31201234567").host(), None);
        assert_eq!(uri("sip:room@example.com").path(), None);
    }

    #[test]
    fn serde_refuses_an_invalid_stored_value() {
        assert!(serde_json::from_str::<ConferenceUri>("\"javascript:x\"").is_err());
        let join = uri("https://meet.example.com/x");
        let json = serde_json::to_string(&join).unwrap();
        assert_eq!(serde_json::from_str::<ConferenceUri>(&json).unwrap(), join);
    }
}
