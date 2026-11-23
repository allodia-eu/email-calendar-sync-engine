//! The out-of-office reply: one provider-neutral model of an account's automatic reply.
//!
//! Every transport the engine speaks keeps one reply per account, as a setting rather than a
//! message: JMAP's `VacationResponse` singleton (RFC 8621 §8), the Sieve `vacation` action
//! (RFC 5230, RFC 6131) in the account's active script, Microsoft Graph's
//! `automaticRepliesSetting`, and Gmail's `VacationSettings`. They differ in what they can say,
//! so the model is the union, and each transport states which parts it can hold:
//!
//! - [`VacationResponse`] is the reply as read: whether it is on, the [`VacationWindow`] it runs
//!   in, the [`AutoReply`] it sends, the [`ReplyAudience`] it answers, an optional second reply for
//!   senders outside the organization, the [`VacationVersion`] a write is guarded by, and the
//!   provider's own form ([`RawVacation`]).
//! - [`VacationVocabulary`] is what one transport can hold, and
//!   [`lost_parts`](VacationVocabulary::lost_parts) is the one rule that says which
//!   [`VacationPart`]s of a reply it would lose, so a settings screen greys out what an account
//!   cannot keep and a write refuses what it would otherwise drop.
//! - [`VacationReceipt`] is what a write answers: the version the reply has now, and what the write
//!   did to the account's own mail rules ([`RulesEffect`]), because on a JMAP server that runs the
//!   reply as a Sieve script, turning the reply on switches the user's own filter script off, and
//!   turning it off leaves it off (RFC 9661 §2.1: at most one active script; observed on Stalwart,
//!   `docs/design/filters-and-out-of-office.md`).
//!
//! The reply's subject and bodies are content: every type here that carries them has a redacted
//! `Debug` that shows only which are present and how long they are.
//!
//! [`VacationVersion`]: crate::version::VacationVersion
//! [`RawVacation`]: crate::raw::RawVacation

mod receipt;
mod reply;
mod vocabulary;

pub use receipt::{RulesEffect, VacationReceipt};
pub use reply::{AutoReply, ReplyAudience, VacationResponse, VacationWindow};
pub use vocabulary::{ReplyBodies, VacationPart, VacationVocabulary, WindowSupport};

#[cfg(test)]
mod tests;
