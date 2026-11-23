//! A rule set and the rules in it.

use super::{Action, Condition};
use crate::{raw::RawRules, version::RuleSetVersion};

/// Defines a string newtype holding content, whose `Debug` shows only its length.
macro_rules! content_text {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            #[doc = "Wraps `value` as it was written."]
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[doc = "The value as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(stringify!($name)).field("len", &self.0.len()).finish()
            }
        }
    };
}

content_text! {
    /// A rule's name as the user gave it: Graph's `displayName`, the `# rule:[name]` comment
    /// of the Sieve dialect the engine writes.
    RuleName
}

content_text! {
    /// A folder as a rule names it by path (a Sieve `fileinto` argument), when the rule names
    /// it neither by role nor by id.
    MailboxPath
}

content_text! {
    /// An address a rule sends mail on to.
    RuleAddress
}

/// An account's mail rules, in evaluation order, as read from its server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuleSet {
    /// The rules, in the order the server applies them.
    pub rules: Vec<MailRule>,
    /// What a write names to be refused if the rules moved since; `None` on a transport that
    /// keeps none.
    pub version: Option<RuleSetVersion>,
    /// The provider's own form, kept so a write copies every rule the caller did not change;
    /// `None` on a set built rather than read.
    pub raw: Option<RawRules>,
}

impl RuleSet {
    /// A set of `rules` (builder-style start; a read adds the version and the raw form).
    #[must_use]
    pub fn new(rules: Vec<MailRule>) -> Self {
        Self {
            rules,
            version: None,
            raw: None,
        }
    }

    /// Records the version the set was read at (builder-style).
    #[must_use]
    pub fn with_version(mut self, version: RuleSetVersion) -> Self {
        self.version = Some(version);
        self
    }

    /// Records the provider's own form (builder-style).
    #[must_use]
    pub fn with_raw(mut self, raw: RawRules) -> Self {
        self.raw = Some(raw);
        self
    }
}

/// One rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MailRule {
    /// The rule's name, where the transport keeps one.
    pub name: Option<RuleName>,
    /// Whether the rule runs. A disabled rule is kept and shown, and does nothing.
    pub enabled: bool,
    /// What the rule does: typed, or carried as the provider wrote it.
    pub body: RuleBody,
    /// Where the rule came from in what was read, so a write can copy an unchanged rule as it
    /// was; `None` on a rule built rather than read.
    pub origin: Option<RuleOrigin>,
}

impl MailRule {
    /// An enabled, unnamed rule that does `actions` to the mail `when` matches.
    #[must_use]
    pub fn typed(when: Match, actions: Vec<Action>) -> Self {
        Self::with_body(RuleBody::Typed(TypedRule {
            when,
            actions,
            stop: false,
        }))
    }

    /// An enabled rule the engine carries but cannot show, kept as `raw`.
    #[must_use]
    pub fn opaque(raw: RawRule) -> Self {
        Self::with_body(RuleBody::Opaque(raw))
    }

    fn with_body(body: RuleBody) -> Self {
        Self {
            name: None,
            enabled: true,
            body,
            origin: None,
        }
    }

    /// Names the rule (builder-style).
    #[must_use]
    pub fn named(mut self, name: RuleName) -> Self {
        self.name = Some(name);
        self
    }

    /// Disables the rule (builder-style).
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Stops evaluating the rules after this one when it matches (builder-style); no effect on
    /// an opaque rule.
    #[must_use]
    pub fn stopping(mut self) -> Self {
        if let RuleBody::Typed(rule) = &mut self.body {
            rule.stop = true;
        }
        self
    }

    /// Records where the rule was read from (builder-style).
    #[must_use]
    pub fn with_origin(mut self, origin: RuleOrigin) -> Self {
        self.origin = Some(origin);
        self
    }
}

/// What a rule does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuleBody {
    /// A rule the engine can show and edit.
    Typed(TypedRule),
    /// A rule the engine carries but cannot show: kept byte for byte, and writable only back to
    /// the same kind of transport.
    Opaque(RawRule),
}

/// A typed rule: what it matches, what it does, and whether evaluation stops after it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TypedRule {
    /// Which mail the rule applies to.
    pub when: Match,
    /// What it does to that mail, in order.
    pub actions: Vec<Action>,
    /// Whether no later rule runs on mail this one matched.
    pub stop: bool,
}

/// Which mail a rule applies to.
///
/// The default is every message: no conditions, and none excepted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Match {
    /// Whether every condition must hold, or any one.
    pub mode: MatchMode,
    /// The conditions; empty: every message.
    pub conditions: Vec<Condition>,
    /// Exceptions: a message any one of these holds for is not matched (Graph's `exceptions`).
    pub except: Vec<Condition>,
}

impl Match {
    /// Mail every one of `conditions` holds for.
    #[must_use]
    pub fn all(conditions: Vec<Condition>) -> Self {
        Self {
            mode: MatchMode::All,
            conditions,
            except: Vec::new(),
        }
    }

    /// Mail any one of `conditions` holds for.
    #[must_use]
    pub fn any(conditions: Vec<Condition>) -> Self {
        Self {
            mode: MatchMode::Any,
            conditions,
            except: Vec::new(),
        }
    }

    /// Leaves out mail any one of `except` holds for (builder-style).
    #[must_use]
    pub fn except(mut self, except: Vec<Condition>) -> Self {
        self.except = except;
        self
    }
}

/// How a rule's conditions combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MatchMode {
    /// Every condition holds.
    #[default]
    All,
    /// At least one condition holds.
    Any,
}

/// The languages a rule may be carried in when the engine cannot type it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RuleSyntax {
    /// Sieve (RFC 5228).
    Sieve,
}

/// A rule the engine carries but cannot show: its language and its exact text.
#[derive(Clone, PartialEq, Eq)]
pub struct RawRule {
    syntax: RuleSyntax,
    text: String,
}

impl RawRule {
    /// A rule in `syntax`, kept as `text`.
    #[must_use]
    pub fn new(syntax: RuleSyntax, text: impl Into<String>) -> Self {
        Self {
            syntax,
            text: text.into(),
        }
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
}

impl core::fmt::Debug for RawRule {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawRule")
            .field("syntax", &self.syntax)
            .field("len", &self.text.len())
            .finish()
    }
}

/// Where a rule was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuleOrigin {
    /// The top-level command at this index of the script read, counting every segment.
    ScriptSegment(usize),
}
