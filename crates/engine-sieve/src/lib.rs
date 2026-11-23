//! `engine-sieve` — Sieve scripts (RFC 5228), split where a rule editor needs them split.
//!
//! The engine models an account's mail rules as a typed, ordered list
//! (`docs/design/filters-and-out-of-office.md`), and a Sieve script is one rendering of it. To
//! edit one rule of a script that someone else may have written by hand, without disturbing the
//! rest, the engine needs to know where each top-level command starts and ends, and nothing
//! more. That is this crate:
//!
//! - [`lex`] — the tokens: strings (quoted and `text:`), comments (hash and bracket), brackets,
//!   identifiers, tags, numbers. Each is a span of the script.
//! - [`segment`](mod@segment) — the script cut at its top-level commands, each [`Segment`] carrying
//!   the comments before it and the rest of its line. The segments' texts in order are the script,
//!   byte for byte, so a writer that replaces some segments and copies the others changes nothing
//!   it did not mean to.
//! - [`require`] — the extensions the `require` commands name, and one `require` naming more.
//! - [`string`] — a string token's value, and a value written as a string.
//!
//! What a command *means* is not here: the crate does not evaluate tests, check that an
//! extension is supported, or know any command but `require` and the `if`/`elsif`/`else` chain.
//! Sieve scripts are hostile input (RFC 9661 §5): every entry point answers an error rather than
//! panicking, and none of its errors or `Debug` output carries the script's text.
//!
//! Stalwart's `sieve-rs` would parse far more, and is `AGPL-3.0-only OR LicenseRef-SEL`, which
//! this MPL-2.0 crate cannot take; it has no dependencies at all.

mod error;
pub mod lex;
pub mod require;
pub mod segment;
pub mod string;

pub use error::{SieveError, SieveErrorKind};
pub use lex::{Token, TokenKind, tokenize};
pub use segment::{Segment, SegmentKind, segment, segment_bytes};
