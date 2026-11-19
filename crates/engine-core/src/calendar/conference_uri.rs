//! The address a conference is joined at.

use std::collections::BTreeSet;

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

    /// The number a `tel:` URI dials, as a person reads it: without its parameters (`;ext=`,
    /// RFC 3966 §5.4) and without what is dialled once the call connects, which services
    /// write after a `,` or `;` pause (`tel:+14255550100,,123456789#`). `None` for any other
    /// scheme, or when nothing comes before the first separator.
    #[must_use]
    pub fn phone_number(&self) -> Option<&str> {
        let number = self.0.strip_prefix("tel:")?;
        let number = number.split([';', ',']).next().unwrap_or_default();
        (!number.is_empty()).then_some(number)
    }

    /// What a `tel:` URI dials once the call connects, which services write after a `,` pause
    /// (`123456789#` in `tel:+14255550100,,123456789#`): the meeting's PIN or access code,
    /// without the pauses before it. `None` for any other scheme, or when nothing follows one.
    #[must_use]
    pub fn dialled_after_pause(&self) -> Option<&str> {
        let (_, after) = self.0.strip_prefix("tel:")?.split_once(',')?;
        let after = after.trim_start_matches(',');
        (!after.is_empty()).then_some(after)
    }

    /// Whether `other` joins the same meeting as this URI.
    ///
    /// Two web URIs do when their host and path match, with percent escapes compared without
    /// regard to case and a trailing `/` ignored, and one's query holds every parameter of
    /// the other's: services write a link both with and without its passcode (Zoom's `pwd`)
    /// or an add-on's marker (`from=addon`), while a parameter that differs can name a
    /// different meeting (Webex's `MTID`). The scheme and fragment are ignored. Any other URI
    /// is the same meeting only as the same text.
    #[must_use]
    pub fn same_meeting(&self, other: &Self) -> bool {
        match (self.meeting_parts(), other.meeting_parts()) {
            (Some((host, path, query)), Some((other_host, other_path, other_query))) => {
                host == other_host
                    && path == other_path
                    && (query.is_subset(&other_query) || other_query.is_subset(&query))
            }
            (None, None) => self.0 == other.0,
            _ => false,
        }
    }

    /// Whether this URI joins the same meeting as `other` ([`Self::same_meeting`]) and carries
    /// every query parameter `other` does and more, such as the passcode (Zoom's `pwd`) that
    /// lets a person join without typing it.
    #[must_use]
    pub fn fuller_than(&self, other: &Self) -> bool {
        match (self.meeting_parts(), other.meeting_parts()) {
            (Some((host, path, query)), Some((other_host, other_path, other_query))) => {
                host == other_host
                    && path == other_path
                    && other_query.is_subset(&query)
                    && query.len() > other_query.len()
            }
            _ => false,
        }
    }

    /// A web URI's host, path and query parameters as [`Self::same_meeting`] compares them.
    fn meeting_parts(&self) -> Option<(String, String, BTreeSet<String>)> {
        let host = self.host()?;
        let path = self.path()?.trim_end_matches('/');
        let without_fragment = self.0.split('#').next().unwrap_or_default();
        let query = without_fragment
            .split_once('?')
            .map_or("", |(_, query)| query)
            .split('&')
            .filter(|param| !param.is_empty())
            .map(upper_escapes)
            .collect();
        Some((host, upper_escapes(path), query))
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

/// `text` with the two hex digits after each `%` in upper case, so `%3a` and `%3A` compare
/// equal.
fn upper_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut upper = 0;
    for c in text.chars() {
        if upper > 0 {
            out.push(c.to_ascii_uppercase());
            upper -= 1;
        } else {
            out.push(c);
            if c == '%' {
                upper = 2;
            }
        }
    }
    out
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
    fn a_phone_number_is_read_without_its_parameters_or_what_is_dialled_after_it() {
        assert_eq!(
            uri("tel:+1-425-555-0100;ext=12").phone_number(),
            Some("+1-425-555-0100")
        );
        assert_eq!(
            uri("tel:+14255550100,,123456789#").phone_number(),
            Some("+14255550100")
        );
        assert_eq!(
            uri("tel:+31 20 555 0100").phone_number(),
            Some("+31205550100")
        );
        assert_eq!(uri("tel:,,123#").phone_number(), None);
        assert_eq!(uri("https://zoom.us/j/1").phone_number(), None);
    }

    #[test]
    fn what_is_dialled_after_the_pause_is_read_without_the_pause_or_the_parameters() {
        assert_eq!(
            uri("tel:+14255550100,,123456789#").dialled_after_pause(),
            Some("123456789#")
        );
        assert_eq!(
            uri("tel:+1-425-555-0100;ext=12,123#").dialled_after_pause(),
            Some("123#")
        );
        assert_eq!(
            uri("tel:+1-425-555-0100;ext=12").dialled_after_pause(),
            None
        );
        assert_eq!(uri("tel:+14255550100").dialled_after_pause(), None);
        assert_eq!(uri("tel:+14255550100,,").dialled_after_pause(), None);
        assert_eq!(uri("https://zoom.us/j/1,2").dialled_after_pause(), None);
    }

    #[test]
    fn a_link_with_more_of_the_meetings_parameters_is_fuller() {
        let bare = uri("https://zoom.us/j/987");
        let full = uri("https://zoom.us/j/987?pwd=x");
        assert!(full.fuller_than(&bare));
        assert!(!bare.fuller_than(&full));
        assert!(!full.fuller_than(&full));
        assert!(!uri("https://zoom.us/j/988?pwd=x").fuller_than(&bare));
    }

    #[test]
    fn the_same_meeting_is_the_same_host_path_and_query_however_written() {
        let teams =
            uri("https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d");
        assert!(teams.same_meeting(&uri(
            "https://Teams.Microsoft.com/l/meetup-join/19%3Ameeting_x/0/?context=%7B%7D#x"
        )));
        let zoom = uri("https://zoom.us/j/123?pwd=abc");
        assert!(zoom.same_meeting(&uri("https://zoom.us/j/123")));
        assert!(!zoom.same_meeting(&uri("https://zoom.us/j/123?pwd=xyz")));
        assert!(!zoom.same_meeting(&uri("https://zoom.us/j/124?pwd=abc")));
        assert!(zoom.same_meeting(&uri("https://zoom.us/j/123?from=addon&pwd=abc")));
        assert!(!zoom.same_meeting(&uri("https://zoom.us/j/123?pwd=xyz&from=addon")));
        assert!(uri("https://zoom.us/j/123#a?pwd=x").same_meeting(&zoom));
        assert!(
            !uri("https://meet.example.com/a").same_meeting(&uri("https://meet.example.com/A"))
        );
        assert!(uri("tel:+3120").same_meeting(&uri("tel:+31 20")));
        assert!(!uri("tel:+3120").same_meeting(&uri("https://meet.example.com/a")));
    }

    #[test]
    fn serde_refuses_an_invalid_stored_value() {
        assert!(serde_json::from_str::<ConferenceUri>("\"javascript:x\"").is_err());
        let join = uri("https://meet.example.com/x");
        let json = serde_json::to_string(&join).unwrap();
        assert_eq!(serde_json::from_str::<ConferenceUri>(&json).unwrap(), join);
    }
}
