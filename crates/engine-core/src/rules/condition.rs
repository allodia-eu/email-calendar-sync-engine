//! What a rule tests a message for.

use super::{NativeCondition, RuleValueError};
use crate::mail::Importance;

/// One thing a rule tests a message for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Condition {
    /// A header field's text.
    Header {
        /// Which field.
        field: HeaderField,
        /// What its text is tested for.
        test: TextTest,
    },
    /// An address field, or a part of each address in it.
    Address {
        /// Which field.
        field: AddressField,
        /// Which part of each address.
        part: AddressPart,
        /// What that part is tested for.
        test: TextTest,
    },
    /// The account's own address is in a recipient field (needs the account's addresses).
    SentToMe(MeField),
    /// The message's text body.
    Body(TextTest),
    /// The message's size.
    Size(SizeBound),
    /// The message has an attachment.
    HasAttachment,
    /// The message states this importance.
    Importance(Importance),
    /// A provider's own condition, carried to the same kind of provider only.
    Native(NativeCondition),
}

/// A header field a rule tests.
///
/// Built with [`named`](Self::named), which reads `Subject` in any case as
/// [`Subject`](Self::Subject), so the one field every transport can test is always spelled one way.
/// Its `Debug` shows a named field's length, not its name, as every rule value's does.
#[derive(Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HeaderField {
    /// `Subject`, which every transport can test.
    Subject,
    /// Any other field, by name as written.
    Named(String),
}

impl HeaderField {
    /// The field called `name`: [`Subject`](Self::Subject) for `Subject` in any case, any other
    /// [`Named`](Self::Named).
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        let name = name.into();
        if name.eq_ignore_ascii_case("subject") {
            Self::Subject
        } else {
            Self::Named(name)
        }
    }

    /// Whether this is the subject, however it was built.
    #[must_use]
    pub fn is_subject(&self) -> bool {
        match self {
            Self::Subject => true,
            Self::Named(name) => name.eq_ignore_ascii_case("subject"),
        }
    }
}

impl core::fmt::Debug for HeaderField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Subject => f.write_str("Subject"),
            Self::Named(name) => f.debug_struct("Named").field("len", &name.len()).finish(),
        }
    }
}

/// What a text is tested for: any one of its values, compared by its operation, ignoring case
/// where the transport does (Sieve's default comparator, `i;ascii-casemap`). It always has a
/// value: a test against none would match nothing anywhere, and no transport writes one.
#[derive(Clone, PartialEq, Eq)]
pub struct TextTest {
    op: TextOp,
    values: Vec<String>,
}

impl TextTest {
    /// A test that holds when the text compares by `op` to any one of `values`.
    ///
    /// # Errors
    ///
    /// [`RuleValueError::Empty`] where `values` is empty.
    pub fn new(op: TextOp, values: Vec<String>) -> Result<Self, RuleValueError> {
        if values.is_empty() {
            return Err(RuleValueError::Empty);
        }
        Ok(Self { op, values })
    }

    /// How the text is compared.
    #[must_use]
    pub fn op(&self) -> TextOp {
        self.op
    }

    /// The values; the test holds when any one does. Never empty.
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

/// Values are content: only the operation and how many values there are.
impl core::fmt::Debug for TextTest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TextTest")
            .field("op", &self.op)
            .field("values", &self.values.len())
            .finish()
    }
}

/// How a text is compared with a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum TextOp {
    /// The text contains the value.
    Contains,
    /// The text is the value.
    Is,
    /// The text matches the value as a glob: `*` any run, `?` one character.
    Matches,
    /// The text matches the value as a regular expression.
    Regex,
}

/// An address field a rule tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AddressField {
    /// `From`.
    From,
    /// `To`.
    To,
    /// `Cc`.
    Cc,
    /// `To` or `Cc`.
    ToOrCc,
}

/// Which part of an address a test looks at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AddressPart {
    /// The whole address.
    #[default]
    All,
    /// What precedes the `@`.
    LocalPart,
    /// What follows the `@`.
    Domain,
}

/// Where the account's own address appears, for [`Condition::SentToMe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MeField {
    /// In `To`.
    To,
    /// In `Cc`.
    Cc,
    /// In `To` or `Cc`.
    ToOrCc,
    /// The only recipient.
    OnlyTo,
    /// Not in `To`.
    NotTo,
}

/// A size bound, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SizeBound {
    /// Larger than this many bytes.
    Over(u64),
    /// Smaller than this many bytes.
    Under(u64),
}
