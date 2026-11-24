//! What a plan or a write says became of each rule handed in: by position and class, never a
//! value.

/// What became of one rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuleOutcome {
    /// The rule's index in the rules handed in.
    pub source: usize,
    /// How much of it the target keeps.
    pub kept: Kept,
    /// Each part lost or changed, and why.
    pub losses: Vec<Loss>,
}

/// How much of a rule a target keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kept {
    /// All of it, some parts perhaps [`Approximated`](LossReason::Approximated).
    Whole,
    /// Without an action, its stop, its name or its order: it does less, never more.
    Reduced,
    /// Stored disabled for review: an imported rule that sends mail out of the account, or one
    /// its source marks in error. What else it lost is in its [`losses`](RuleOutcome::losses),
    /// as for any rule.
    Disabled,
    /// None of it: what was lost would have widened what it matches, or left it nothing to do.
    NotKept,
}

/// One part of a rule lost or changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Loss {
    /// Which part.
    pub part: RulePart,
    /// Why.
    pub reason: LossReason,
}

impl Loss {
    /// `part` lost or changed, for `reason`: what an adapter adds to a planned outcome for what
    /// only it can find it cannot write.
    #[must_use]
    pub fn new(part: RulePart, reason: LossReason) -> Self {
        Self { part, reason }
    }
}

/// A part of a rule, by position: never its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RulePart {
    /// The whole rule.
    Rule,
    /// How its conditions combine.
    Mode,
    /// Its condition at this index.
    Condition(usize),
    /// Its exception at this index.
    Exception(usize),
    /// Its action at this index.
    Action(usize),
    /// Its stop.
    Stop,
    /// Whether it runs.
    Enabled,
    /// Its name.
    Name,
    /// Its place in the order.
    Order,
}

/// Why a part was lost or changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LossReason {
    /// The target's language has no equivalent.
    NotExpressible,
    /// Carried with a stated difference.
    Approximated,
    /// The folder it names has no counterpart on the target: an id the host's folder map does
    /// not hold ([`RuleVocabulary::with_folders`](super::RuleVocabulary::with_folders)), or a rule
    /// carried whole that names a folder by id.
    MissingMailbox,
    /// The target refuses to send mail on there: too many addresses, an address it has not
    /// verified, or a forwarding rule an import arms only on request.
    ForwardNotAllowed,
    /// Over a limit of the target: the rule count, the script size.
    LimitExceeded,
    /// The source itself marks the rule as in error (Graph `hasError`), on the whole rule. An
    /// edit writes it as given; an import stores it disabled, or not at all where the target
    /// cannot store a disabled rule. Not a loss of the target's, so it alone leaves the rule
    /// [`Whole`](Kept::Whole).
    SourceInError,
    /// A rule the engine could not read, going to another kind of transport.
    OpaqueSource,
    /// The target was asked to store the part and did not: a transport that stores rules one by
    /// one went on past it, and the receipt's [`failures`](super::RulesReceipt::failures) say
    /// why. On the whole rule it is not kept as given; on its order, it is reduced.
    NotWritten,
}
