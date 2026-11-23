//! The text values a rule carries by name: what it is called, a folder path, an address.

use core::fmt;

/// Why a text value cannot be one a rule carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuleValueError {
    /// It is empty.
    Empty,
    /// It holds a CR, an LF or a NUL, which no rule language this engine writes can carry in
    /// such a value: a name is one comment line, a path and an address one string.
    LineBreak,
}

impl fmt::Display for RuleValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "a rule value is empty",
            Self::LineBreak => "a rule value holds a line break or a NUL",
        })
    }
}

impl std::error::Error for RuleValueError {}

/// Defines a string newtype holding content: never empty, never a CR, LF or NUL, and a `Debug`
/// that shows only its length.
macro_rules! rule_value {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            #[doc = "Wraps `value` as it was written."]
            #[doc = ""]
            #[doc = "# Errors"]
            #[doc = ""]
            #[doc = "[`RuleValueError`] where it is empty, or holds a CR, an LF or a NUL."]
            pub fn new(value: impl Into<String>) -> Result<Self, RuleValueError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(RuleValueError::Empty);
                }
                if value.contains(['\r', '\n', '\0']) {
                    return Err(RuleValueError::LineBreak);
                }
                Ok(Self(value))
            }

            #[doc = "The value as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).field("len", &self.0.len()).finish()
            }
        }
    };
}

rule_value! {
    /// A rule's name as the user gave it: Graph's `displayName`, the `# rule:[name]` comment
    /// line of the Sieve dialect the engine writes.
    RuleName
}

rule_value! {
    /// A folder as a rule names it by path, when it names it neither by role nor by id: a Sieve
    /// `fileinto` argument, in the server's own hierarchy separator.
    MailboxPath
}

rule_value! {
    /// An address a rule sends mail on to, as the rule gives it; not checked to be one.
    RuleAddress
}
