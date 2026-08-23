//! `engine-http` — one throttling answer for every HTTP provider.
//!
//! Gmail, Graph, JMAP and CalDAV each classify a rate limit correctly, and each then returns
//! it to the caller unchanged. Nothing waits. That is survivable while an adapter issues one
//! request at a time and simply cannot outrun a limit; it stops being survivable the moment a
//! pass fans out, because the first thing a wide pass meets is the ceiling it was widened
//! towards.
//!
//! So the wait belongs here rather than in an adapter: the four transports differ in how they
//! authenticate and what they parse, and in nothing that decides how long to wait for a `429`.
//! A host that wanted the behaviour uniform would otherwise be relying on four separate
//! implementations happening to agree.
//!
//! # What it does
//!
//! [`send_retrying`] wraps a prepared `reqwest` request. On a throttled reply it waits and
//! sends the request again, up to a bounded number of attempts and a bounded total wait:
//!
//! - **The server's own `Retry-After` wins** where one is supplied. Guessing shorter than a number
//!   the server named is what turns one throttle into several.
//! - **Jitter is added either way**, because the requests being throttled are concurrent — twenty
//!   of them backing off by an identical amount retry in lockstep and throttle again.
//! - **`429` is retried whatever the method.** The reply means the request was refused, not that it
//!   was applied, so replaying it cannot duplicate anything.
//! - **`503` is retried only for an idempotent method.** There the outcome is genuinely unknown,
//!   and a replayed `POST` is a message sent twice.
//!
//! # Reporting
//!
//! The engine has no logger — a host owns its own I/O — so a wait that a user would otherwise
//! experience as an unexplained stall is reported through [`ThrottleObserver`], which the host
//! implements and logs. A [`ThrottleEvent`] carries the provider label, the status, the attempt
//! and the delay, and deliberately carries no URL: a request path names the user's own mail.

mod observer;
mod policy;
mod send;

pub use observer::{IgnoreThrottles, ThrottleEvent, ThrottleObserver};
pub use policy::RetryPolicy;
pub use send::{RetryConfig, send_retrying};

#[cfg(test)]
mod send_tests;
