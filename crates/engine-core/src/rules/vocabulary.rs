//! What one account's rule language can express.

use std::collections::{BTreeMap, BTreeSet};

use super::{FolderMap, NativeKind, RawRule, RuleSyntax, TextOp};

/// One thing a rule can say, as a target either can or cannot hold it.
///
/// The grain is where the transports differ (`filters-and-out-of-office.md`, §5): Sieve tests a
/// header with a regular expression only where `regex` is listed, Graph cannot test any header
/// but by "contains", Gmail cannot stop. A feature added later is an addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RuleFeature {
    /// Conditions combined so that any one suffices.
    MatchAny,
    /// Exceptions that veto the rule.
    Except,
    /// The subject tested by an operation.
    SubjectText(TextOp),
    /// Another header field tested by an operation.
    HeaderText(TextOp),
    /// A whole address tested by an operation.
    AddressText(TextOp),
    /// The local part of an address tested.
    AddressLocalPart,
    /// The domain of an address tested.
    AddressDomain,
    /// The account's own address among the recipients.
    SentToMe,
    /// The text body tested by an operation.
    Body(TextOp),
    /// Size larger than a bound.
    SizeOver,
    /// Size smaller than a bound.
    SizeUnder,
    /// Has an attachment.
    HasAttachment,
    /// States an importance.
    Importance,
    /// Files into a folder named by path.
    FileInto,
    /// Files a copy, keeping the message where it was.
    FileIntoCopy,
    /// Files into the folder with a role.
    FileIntoRole,
    /// Files into a folder by id.
    FileIntoId,
    /// Deletes for good.
    Discard,
    /// Sends on, the original sender kept.
    Redirect,
    /// Sends on and keeps it.
    RedirectCopy,
    /// Sends on as the account.
    Forward,
    /// Marks read (`$seen`).
    MarkRead,
    /// Flags (`$flagged`).
    Flag,
    /// Adds any other keyword.
    NamedKeyword,
    /// Marks an importance.
    MarkImportance,
    /// Stops evaluating later rules.
    Stop,
    /// Stores a rule that does not run.
    Disabled,
    /// Stores a rule that does not run, whose test holds a value that spans lines (the Sieve
    /// dialect keeps a disabled rule's test in a comment, which ends at its line's end).
    DisabledMultiLine,
    /// Stores a rule's name.
    Name,
    /// Applies rules in the order given.
    Order,
}

/// A limit on how parts a target holds one by one can be combined in one rule.
///
/// Where a target keeps a rule's tests as keys of one object (Graph's `messageRulePredicates`),
/// two tests of one kind cannot both be kept, and an action that names one thing cannot be
/// there twice. A target with no limits combines freely, as a Sieve script does; a limit added
/// later is an addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RuleLimit {
    /// At most one test of each kind among the conditions, and one among the exceptions: the
    /// target keeps one test per kind, its values any-of. A kind is the condition with its field,
    /// part and operation: a subject that contains, a sender that is, an importance.
    OneTestPerKind,
    /// At most one action of each kind that names one thing: one move, one copy, one importance.
    OneActionPerKind,
    /// `To` and `Cc` are tested only together, as to-or-cc.
    RecipientsTogether,
    /// No system keyword is added but `$seen` and `$flagged`.
    NoOtherSystemKeyword,
}

/// How a target holds a feature it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Support {
    /// As the model means it.
    Exact,
    /// With a stated difference: Sieve's `addheader "Importance"` for Graph's importance, a
    /// `redirect :copy` for Exchange's forward.
    Approximated,
}

/// What an account's rule language can express: which [`RuleFeature`]s it holds and how, how
/// many addresses one rule may send a message to, which kinds of carried rule it can store back,
/// and the limits on combining them ([`RuleLimit`]).
///
/// Read per account, because two servers speaking one protocol list different extensions
/// (`filters-and-out-of-office.md`, "Observed on the harness"); a constant for a transport whose
/// language is fixed. Built with [`new`](Self::new) and the `with_*` methods; its fields are
/// private, so what a later transport needs is an addition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleVocabulary {
    features: BTreeMap<RuleFeature, Support>,
    max_redirects: Option<usize>,
    opaque: Option<(RuleSyntax, BTreeSet<String>)>,
    native: Option<NativeKind>,
    folders: Option<FolderMap>,
    limits: BTreeSet<RuleLimit>,
}

impl RuleVocabulary {
    /// A language that can express nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds `feature` as `support` says (builder-style).
    #[must_use]
    pub fn with(mut self, feature: RuleFeature, support: Support) -> Self {
        self.features.insert(feature, support);
        self
    }

    /// Holds every one of `features` exactly (builder-style).
    #[must_use]
    pub fn with_exact(mut self, features: impl IntoIterator<Item = RuleFeature>) -> Self {
        for feature in features {
            self.features.insert(feature, Support::Exact);
        }
        self
    }

    /// Holds `feature` no longer (builder-style): for a transport that cannot use what its
    /// language could say (filing by id where the account's folder ids are not the language's).
    #[must_use]
    pub fn without(mut self, feature: RuleFeature) -> Self {
        self.features.remove(&feature);
        self
    }

    /// Lets one message be sent on to at most `max` addresses (builder-style): JMAP Sieve's
    /// `maxNumberRedirects`.
    #[must_use]
    pub fn with_max_redirects(mut self, max: usize) -> Self {
        self.max_redirects = Some(max);
        self
    }

    /// Stores rules carried in `syntax` back as they were, where they require only
    /// `extensions` (builder-style): the extensions the account lists.
    #[must_use]
    pub fn with_opaque(
        mut self,
        syntax: RuleSyntax,
        extensions: impl IntoIterator<Item = String>,
    ) -> Self {
        self.opaque = Some((syntax, extensions.into_iter().collect()));
        self
    }

    /// Stores `kind`'s own conditions and actions (builder-style).
    #[must_use]
    pub fn with_native(mut self, kind: NativeKind) -> Self {
        self.native = Some(kind);
        self
    }

    /// Files by id only into the target folders `folders` maps a source folder to, each written
    /// as the target's id (builder-style): what an importer knows of its pair. A rule naming a
    /// folder the map does not hold loses that action as
    /// [`MissingMailbox`](super::LossReason::MissingMailbox), rather than filing into whatever
    /// folder of the target's own has that id. Without it, an id is the account's own, as an edit
    /// has it.
    #[must_use]
    pub fn with_folders(mut self, folders: FolderMap) -> Self {
        self.folders = Some(folders);
        self
    }

    /// The source folders an import maps to the target's ([`with_folders`](Self::with_folders)).
    #[must_use]
    pub fn folders(&self) -> Option<&FolderMap> {
        self.folders.as_ref()
    }

    /// Holds parts only within `limit` (builder-style).
    #[must_use]
    pub fn with_limit(mut self, limit: RuleLimit) -> Self {
        self.limits.insert(limit);
        self
    }

    /// Whether parts combine only within `limit`.
    #[must_use]
    pub fn has_limit(&self, limit: RuleLimit) -> bool {
        self.limits.contains(&limit)
    }

    /// How the language holds `feature`, or `None` where it cannot.
    #[must_use]
    pub fn support(&self, feature: RuleFeature) -> Option<Support> {
        self.features.get(&feature).copied()
    }

    /// How many addresses one message may be sent on to, or `None` for no stated limit.
    #[must_use]
    pub fn max_redirects(&self) -> Option<usize> {
        self.max_redirects
    }

    /// Whether `raw` can be stored back: it is in the language this account speaks, and what
    /// it requires is known and listed.
    #[must_use]
    pub fn stores_opaque(&self, raw: &RawRule) -> bool {
        self.opaque.as_ref().is_some_and(|(syntax, listed)| {
            *syntax == raw.syntax()
                && raw
                    .requires()
                    .is_some_and(|requires| requires.is_subset(listed))
        })
    }

    /// Whether `kind`'s own conditions and actions can be stored.
    #[must_use]
    pub fn stores_native(&self, kind: NativeKind) -> bool {
        self.native == Some(kind)
    }
}
