//! What a rule does to the mail it matches, and the provider-own parts a rule can carry.

use std::collections::BTreeSet;

use super::{MailboxPath, RuleAddress};
use crate::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole},
};

/// Something a rule does to the mail it matches.
///
/// Its `Debug` shows the kind of each part and how many values it has, never a folder, an
/// address or a keyword.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Files the message into a folder: moves it, or with `copy` files a copy and keeps it where
    /// it was.
    FileInto {
        /// The folder.
        mailbox: RuleMailbox,
        /// Whether the message also stays where it was.
        copy: bool,
    },
    /// Deletes the message for good, not to Trash.
    Discard,
    /// Sends the message on, its original sender kept; with `copy`, the account keeps it too.
    Redirect {
        /// Where it goes.
        to: Vec<RuleAddress>,
        /// Whether the account keeps it as well.
        copy: bool,
    },
    /// Sends the message on as the account (Exchange's forward), keeping it.
    Forward {
        /// Where it goes.
        to: Vec<RuleAddress>,
    },
    /// Adds keywords: `$seen` marks it read, `$flagged` flags it, any other names a keyword.
    AddKeywords(BTreeSet<Keyword>),
    /// Marks the message with an importance.
    MarkImportance(Importance),
    /// A provider's own action, carried to the same kind of provider only.
    Native(NativeAction),
}

impl core::fmt::Debug for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FileInto { mailbox, copy } => f
                .debug_struct("FileInto")
                .field("mailbox", mailbox)
                .field("copy", copy)
                .finish(),
            Self::Discard => f.write_str("Discard"),
            Self::Redirect { to, copy } => f
                .debug_struct("Redirect")
                .field("to", &to.len())
                .field("copy", copy)
                .finish(),
            Self::Forward { to } => f.debug_struct("Forward").field("to", &to.len()).finish(),
            Self::AddKeywords(keywords) => {
                f.debug_tuple("AddKeywords").field(&keywords.len()).finish()
            }
            Self::MarkImportance(importance) => {
                f.debug_tuple("MarkImportance").field(importance).finish()
            }
            Self::Native(native) => f.debug_tuple("Native").field(native).finish(),
        }
    }
}

/// A folder a rule files into.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuleMailbox {
    /// The account's folder with this role, whatever it is called.
    Role(MailboxRole),
    /// The folder with this id, whatever it is called now.
    Id(MailboxId),
    /// The folder at this path, as the rule names it.
    Path(MailboxPath),
}

/// The kinds of provider whose own conditions and actions a rule can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum NativeKind {
    /// Microsoft Graph `messageRule` JSON.
    Graph,
    /// A Gmail filter's JSON.
    Gmail,
}

/// A provider's own condition: its kind, and its JSON fragment as read.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeCondition {
    /// Whose condition it is.
    pub kind: NativeKind,
    /// The provider's JSON fragment.
    pub raw: String,
}

/// A provider's own action: its kind, and its JSON fragment as read.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeAction {
    /// Whose action it is.
    pub kind: NativeKind,
    /// The provider's JSON fragment.
    pub raw: String,
}

macro_rules! native_debug {
    ($name:ident) => {
        impl $name {
            /// A fragment of `kind`'s own JSON.
            #[must_use]
            pub fn new(kind: NativeKind, raw: impl Into<String>) -> Self {
                Self {
                    kind,
                    raw: raw.into(),
                }
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("kind", &self.kind)
                    .field("len", &self.raw.len())
                    .finish()
            }
        }
    };
}

native_debug!(NativeCondition);
native_debug!(NativeAction);
