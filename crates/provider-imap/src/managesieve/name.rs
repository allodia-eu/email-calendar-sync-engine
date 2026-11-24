//! [`ScriptName`]: the name of a Sieve script on a ManageSieve server.

use core::fmt;

use crate::error::{ImapError, ImapResult};

/// The name of one of an account's Sieve scripts.
///
/// RFC 5804 §1.6 allows any UTF-8 but control characters and the line and paragraph separators
/// (U+2028, U+2029), so a name is never empty and never holds one. Its `Debug` shows only its
/// length, because a user names scripts.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ScriptName(String);

impl ScriptName {
    /// A name the engine chose, or one a server listed.
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] for an empty name or one holding a control character.
    pub(crate) fn new(name: &str) -> ImapResult<Self> {
        if name.is_empty()
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        {
            return Err(ImapError::protocol(
                "a Sieve script name that is empty or holds a control character or line separator",
            ));
        }
        Ok(Self(name.to_owned()))
    }

    /// A name a server listed that RFC 5804 §1.6 does not allow: kept to count and hash the
    /// script, never sent back.
    pub(crate) fn forbidden(name: &str) -> Self {
        Self(name.to_owned())
    }

    /// The name, as the protocol carries it.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ScriptName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ScriptName(<{} bytes>)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_never_empty_and_never_holds_a_control_character() {
        assert_eq!(ScriptName::new("rules").unwrap().as_str(), "rules");
        assert_eq!(
            ScriptName::new("Règles été").unwrap().as_str(),
            "Règles été"
        );
        for bad in [
            "",
            "a\r\nb",
            "a\0b",
            "tab\there",
            "a\u{2028}b",
            "a\u{2029}b",
        ] {
            assert!(ScriptName::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn debug_shows_only_a_length() {
        let shown = format!("{:?}", ScriptName::new("Holiday plans").unwrap());
        assert_eq!(shown, "ScriptName(<13 bytes>)");
    }
}
