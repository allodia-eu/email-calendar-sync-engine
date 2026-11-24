//! Mail rules: one provider-neutral model of an account's filters.
//!
//! Every transport the engine speaks keeps an account's rules on the server, in a language of
//! its own: a Sieve script over JMAP (RFC 9661) or ManageSieve (RFC 5804), Microsoft Graph's
//! `messageRule`s, Gmail's filters. The model is a typed, ordered rule list, and each language
//! is one rendering of it (`docs/design/filters-and-out-of-office.md`, §3):
//!
//! - [`RuleSet`] is the account's rules as read, in evaluation order, with the [`RuleSetVersion`] a
//!   write is guarded by and the provider's own form ([`RawRules`]), from which a write copies
//!   every rule the caller did not change.
//! - [`MailRule`] is one rule: its name, whether it runs, and its [`RuleBody`], either a
//!   [`TypedRule`] ([`Match`], [`Action`]s, stop) or a rule the engine carries but cannot show,
//!   kept byte for byte ([`RawRule`]).
//! - [`RuleVocabulary`] is what one account's rule language can express, per [`RuleFeature`].
//! - [`plan_rules`] is the one rule for what a target keeps of a set, before anything is sent, and
//!   [`RulesPlan`] / [`RuleOutcome`] say it per rule, by position and class; [`held_forwarding`]
//!   lists, for a transfer host's preflight, the rules an import holds back because they send mail
//!   out of the account.
//! - [`RulesReceipt`] is what a write answers.
//!
//! Rule values, names, addresses and folder paths are content: every type here that carries
//! them has a `Debug` that shows only kinds, counts and lengths.
//!
//! [`RuleSetVersion`]: crate::version::RuleSetVersion
//! [`RawRules`]: crate::raw::RawRules

mod action;
mod condition;
mod folders;
mod plan;
mod plan_support;
mod preflight;
mod raw_rule;
mod receipt;
mod rule;
mod values;
mod vocabulary;

pub use action::{Action, NativeAction, NativeCondition, NativeKind, RuleMailbox};
pub use condition::{
    AddressField, AddressPart, Condition, HeaderField, MeField, SizeBound, TextOp, TextTest,
};
pub use folders::FolderMap;
pub use plan::{
    ForwardingRules, Kept, Loss, LossReason, PlanMode, RuleOutcome, RulePart, RulesPlan, plan_rules,
};
pub use preflight::held_forwarding;
pub use raw_rule::{Expansion, RawRule, RuleOrigin, RuleSyntax};
pub use receipt::{FailedRule, ReplyEffect, RuleWriteFailure, RulesReceipt, RulesStored};
pub use rule::{MailRule, Match, MatchMode, RuleBody, RuleSet, RulesActivation, TypedRule};
pub use values::{MailboxPath, RuleAddress, RuleName, RuleValueError};
pub use vocabulary::{RuleFeature, RuleLimit, RuleVocabulary, Support};

#[cfg(test)]
mod import_plan_tests;
#[cfg(test)]
mod plan_limit_tests;
#[cfg(test)]
mod plan_review_tests;
#[cfg(test)]
mod plan_source_tests;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod tests;
