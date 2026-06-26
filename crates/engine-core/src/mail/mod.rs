//! The normalized mail domain model.
//!
//! A [`Message`] is a provider object identified by a [`crate::ids::MessageId`],
//! belonging to a non-empty set of [`Mailbox`] collections via
//! [`crate::membership::Memberships`], carrying [`Keyword`]s as its state axis,
//! and projecting the RFC 5322 headers into a typed [`Envelope`]. Collections
//! carry a normalized [`MailboxRole`] distinct from their id and name. Threads
//! carry [`ThreadProvenance`].
//!
//! The three axes are kept separate throughout: object identity, collection
//! membership, and keyword state. See `modeling.md`.

mod address;
mod body;
mod content;
mod header;
mod keyword;
mod mailbox;
mod message;
mod role;
mod thread;

pub use address::{EmailAddress, EmailAddressGroup};
pub use body::EmailBodyPart;
pub use content::MessageBody;
pub use header::{EmailHeader, Envelope};
pub use keyword::{Keyword, KeywordError, SystemKeyword};
pub use mailbox::Mailbox;
pub use message::Message;
pub use role::MailboxRole;
pub use thread::{Thread, ThreadProvenance};
