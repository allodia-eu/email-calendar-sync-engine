//! What a rule tests a message for.

use super::NativeCondition;
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HeaderField {
    /// `Subject`, which every transport can test.
    Subject,
    /// Any other field, by name as written.
    Named(String),
}

/// What a text is tested for: any one of `values`, compared by `op`, ignoring case where the
/// transport does (Sieve's default comparator, `i;ascii-casemap`).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextTest {
    /// How the text is compared.
    pub op: TextOp,
    /// The values; the test holds when any one does.
    pub values: Vec<String>,
}

impl TextTest {
    /// A test that holds when the text compares by `op` to any one of `values`.
    #[must_use]
    pub fn new(op: TextOp, values: Vec<String>) -> Self {
        Self { op, values }
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
pub enum SizeBound {
    /// Larger than this many bytes.
    Over(u64),
    /// Smaller than this many bytes.
    Under(u64),
}
