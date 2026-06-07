//! `engine-search` — the store-agnostic search layer for the PIM sync engine.
//!
//! Search splits into a portable half and a per-store half. This crate is the
//! portable half: the structured **query AST** (mail and calendar filters plus
//! free text), its **DSL parser**, **reciprocal-rank fusion** (RRF) for merging
//! ranked candidate lists, and **coverage assembly** onto
//! [`engine_core::coverage::SearchCoverage`]. None of it touches a store, a
//! runtime, or SQL.
//!
//! The per-store half — compiling the AST to native filters, FTS (SQLite FTS5
//! `bm25()`, Postgres `tsvector`), and vector KNN, then ranking — lives in each
//! store crate (`store-sqlite`). Keeping the AST and RRF here lets a Postgres
//! adapter reuse them unchanged: only the execution SQL is store-specific
//! (`store-and-sync.md`, `search-coverage.md`).
//!
//! # Shape
//!
//! - [`query`] — the AST. A [`Query`] is one domain (mail *or* calendar); the
//!   vocabularies are disjoint and execute against different indexes, so they are
//!   distinct variants rather than one bag of optional fields. Free text is a
//!   shared [`TextQuery`] of unscoped terms plus field-scoped terms.
//! - [`parse`] — the textual DSL (`from:a subject:"q report" before:2026-01-01`)
//!   parsed into the AST. Only known keywords are operators; everything else is
//!   free text, so URLs and ratios never become spurious filters.
//! - [`rrf`] — reciprocal-rank fusion: merge ranked candidate lists (FTS, vector)
//!   into one ranking, store-agnostic and generic over the candidate key.
//! - [`coverage`] — assemble per-scope coverage facts into one answer's
//!   [`engine_core::coverage::SearchCoverage`].
//! - [`result`] — the ranked [`SearchResults`] an executor returns: object keys
//!   plus the coverage of the answer.

pub mod coverage;
pub mod parse;
pub mod query;
pub mod result;
pub mod rrf;

pub use coverage::assemble;
pub use parse::ParseError;
pub use query::{CalendarQuery, MailQuery, Query, ScopedTerm, TextField, TextQuery};
pub use result::{SearchHit, SearchResults};
pub use rrf::{Fused, RrfK, fuse};
