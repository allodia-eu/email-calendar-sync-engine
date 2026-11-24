//! [`ServerText`]: what a server said in an error reply, kept for a host that asks for it and
//! shown nowhere.
//!
//! Split from `error.rs`, which classifies the errors that carry it.

/// Text a server sent in an error reply: a problem-details document, an HTML page, anything.
///
/// It may echo what the request carried (a subject, an address), and the engine never logs or
/// shows content, so it appears in no `Display` and its `Debug` shows only its length. A host
/// that deliberately wants it reads it with [`as_str`](Self::as_str), knowing what it may hold.
///
/// # Examples
///
/// ```
/// use provider_jmap::ServerText;
///
/// let body = ServerText::from(r#"{"type":"urn:ietf:params:jmap:error:limit"}"#);
/// assert_eq!(format!("{body:?}"), "ServerText(43 bytes)");
/// assert_eq!(
///     body.problem_type(),
///     Some("urn:ietf:params:jmap:error:limit")
/// );
/// ```
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ServerText(String);

impl ServerText {
    /// The text as the server sent it, which may contain content the request carried.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The problem-details `type` (RFC 7807, which RFC 8620 §3.6.1 uses) where the text is such
    /// a document and its `type` is URI-shaped: a code, safe to show. `None` otherwise.
    #[must_use]
    pub fn problem_type(&self) -> Option<&str> {
        let start = self.0.find("\"type\"")?;
        let rest = self.0[start + 6..]
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        let value = rest.strip_prefix('"')?;
        let value = &value[..value.find('"')?];
        let uri_shaped = !value.is_empty()
            && value.len() <= 200
            && value.contains(':')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b":._/-#?=&%~+".contains(&b));
        uri_shaped.then_some(value)
    }
}

impl From<String> for ServerText {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for ServerText {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl core::fmt::Debug for ServerText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ServerText({} bytes)", self.0.len())
    }
}
