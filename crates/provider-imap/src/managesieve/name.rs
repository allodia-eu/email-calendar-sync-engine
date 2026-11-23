//! [`ScriptName`]: the name of a Sieve script on a ManageSieve server.

use core::fmt;

use crate::error::{ImapError, ImapResult};

/// The name of one of an account's Sieve scripts.
///
/// RFC 5804 §1.6 allows any UTF-8 but control characters, so a name is never empty and never
/// holds one: a name read from the server that does is refused rather than sent back. Its
/// `Debug` shows only its length, because a user names scripts.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ScriptName(String);

impl ScriptName {
    /// A name the engine chose, or one a server listed.
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] for an empty name or one holding a control character.
    pub(crate) fn new(name: &str) -> ImapResult<Self> {
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(ImapError::protocol(
                "a Sieve script name that is empty or holds a control character",
            ));
        }
        Ok(Self(name.to_owned()))
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
        for bad in ["", "a\r\nb", "a\0b", "tab\there"] {
            assert!(ScriptName::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn debug_shows_only_a_length() {
        let shown = format!("{:?}", ScriptName::new("Holiday plans").unwrap());
        assert_eq!(shown, "ScriptName(<13 bytes>)");
    }
}
