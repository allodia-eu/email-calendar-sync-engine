//! A rule the engine carries but cannot show.

use core::fmt;
use std::collections::BTreeSet;

use super::RuleName;

/// The languages a rule may be carried in when the engine cannot type it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RuleSyntax {
    /// Sieve (RFC 5228).
    Sieve,
}

/// A rule the engine carries but cannot show: its language, its exact text, and what the
/// reader that found it learned about it, which is what the plan needs to carry it safely.
///
/// - **What it requires**: the extensions its text uses, so a target that lacks one is not handed
///   it (`None`: not known, and no target is).
/// - **Whether it may send mail** out of the account (a Sieve `redirect`, or `notify`), so an
///   import that arms no forwarding rule does not carry it: its text cannot be disabled. Assumed
///   until the reader says otherwise.
/// - **Whether it may name a folder by id or run another script**, which are its own account's, so
///   an import does not carry it. Assumed too, until the reader says otherwise.
/// - **The name it carries**, as read, which its text holds and a write cannot change.
/// - **Whether its strings hold `${`**, and whether the script it came from expanded them
///   ([`Expansion`]): such a rule means something else in a script that expands differently, and
///   one that uses a variable may depend on a command elsewhere in its script.
#[derive(Clone, PartialEq, Eq)]
pub struct RawRule {
    syntax: RuleSyntax,
    text: String,
    requires: Option<BTreeSet<String>>,
    sends_mail: bool,
    name: Option<RuleName>,
    expansion: Option<Expansion>,
    folders_by_id: bool,
    includes: bool,
}

/// How a script reads the `${…}` a carried rule's strings hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Expansion {
    /// As written: the script requires neither `variables` nor `encoded-character`.
    Literal,
    /// Expanded: the script requires `variables` or `encoded-character` (RFC 5229, RFC 5228
    /// §2.4.2.4).
    Expanded,
}

impl RawRule {
    /// A rule in `syntax`, kept as `text`, of which nothing else is known yet.
    #[must_use]
    pub fn new(syntax: RuleSyntax, text: impl Into<String>) -> Self {
        Self {
            syntax,
            text: text.into(),
            requires: None,
            sends_mail: true,
            name: None,
            expansion: None,
            folders_by_id: true,
            includes: true,
        }
    }

    /// Records that the text's strings hold `${`, read as `expansion` says (builder-style).
    #[must_use]
    pub fn holding(mut self, expansion: Expansion) -> Self {
        self.expansion = Some(expansion);
        self
    }

    /// How the script the text came from reads the `${` its strings hold, or `None` where they
    /// hold none.
    #[must_use]
    pub fn expansion(&self) -> Option<Expansion> {
        self.expansion
    }

    /// Records the extensions the text requires (builder-style).
    #[must_use]
    pub fn requiring(mut self, extensions: impl IntoIterator<Item = String>) -> Self {
        self.requires = Some(extensions.into_iter().collect());
        self
    }

    /// Records that the text sends no mail out of the account (builder-style).
    #[must_use]
    pub fn sending_no_mail(mut self) -> Self {
        self.sends_mail = false;
        self
    }

    /// Records the name the text carries (builder-style).
    #[must_use]
    pub fn named(mut self, name: RuleName) -> Self {
        self.name = Some(name);
        self
    }

    /// Records that the text names no folder by its id (builder-style). Until a reader says so,
    /// it is assumed to (Sieve's `fileinto :mailboxid`), as sending mail is: an import does not
    /// carry what it cannot vouch for.
    #[must_use]
    pub fn naming_no_folder_by_id(mut self) -> Self {
        self.folders_by_id = false;
        self
    }

    /// Whether the text may name a folder by its id, which is its own account's: an import does
    /// not carry it.
    #[must_use]
    pub fn names_folders_by_id(&self) -> bool {
        self.folders_by_id
    }

    /// Records that the text runs no other script of its account (builder-style). Until a reader
    /// says so, it is assumed to (Sieve's `include`).
    #[must_use]
    pub fn including_no_script(mut self) -> Self {
        self.includes = false;
        self
    }

    /// Whether the text may run another script of its account: an import does not carry it, since
    /// the script is the source's.
    #[must_use]
    pub fn includes_scripts(&self) -> bool {
        self.includes
    }

    /// The rule's language.
    #[must_use]
    pub fn syntax(&self) -> RuleSyntax {
        self.syntax
    }

    /// The rule's exact text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The extensions the text requires, or `None` where they are not known.
    #[must_use]
    pub fn requires(&self) -> Option<&BTreeSet<String>> {
        self.requires.as_ref()
    }

    /// Whether the text may send mail out of the account.
    #[must_use]
    pub fn sends_mail(&self) -> bool {
        self.sends_mail
    }

    /// The name the text carries.
    #[must_use]
    pub fn name(&self) -> Option<&RuleName> {
        self.name.as_ref()
    }
}

impl fmt::Debug for RawRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawRule")
            .field("syntax", &self.syntax)
            .field("len", &self.text.len())
            .field("requires", &self.requires)
            .field("sends_mail", &self.sends_mail)
            .field("folders_by_id", &self.folders_by_id)
            .field("includes", &self.includes)
            .finish_non_exhaustive()
    }
}

/// Where a rule was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuleOrigin {
    /// A top-level command of a script.
    ScriptSegment {
        /// A fingerprint of the script's text, so a rule read from one script is never taken
        /// for a command of another.
        script: u64,
        /// The command's index among the script's segments, counting every segment.
        index: usize,
    },
    /// One of the separate objects a provider lists the rules as (Graph's `messageRule`s): the
    /// object at `index` of the listing kept in the set's raw form, in the order read. The
    /// provider's own id stays there, so the origin is a position and never a value.
    Listed {
        /// A fingerprint of the listing read, so a rule read from one listing is never taken
        /// for an object of another.
        set: u64,
        /// The object's index in that listing.
        index: usize,
    },
}
