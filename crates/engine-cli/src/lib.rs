//! `engine-cli` — the headless ingestion/search harness.
//!
//! This is the CLI host from `north-star.md`: a thin orchestration layer over the
//! engine crates that opens/migrates a [`store_sqlite::SqliteStore`], ingests JSON
//! fixtures (project → expand → apply), advances the occurrence horizon, and runs
//! DSL queries — so the whole local pipeline is exercisable from a shell and is the
//! home for recurrence fixtures.
//!
//! The library half (this crate's lib) holds the testable pipeline; the binary
//! (`main.rs`) is argument parsing over it. When `engine-api` lands, the CLI will
//! consume that stable facade instead of the store directly.
//!
//! Fixtures are JSON of **normalized** engine-core objects (`Message`/`Event`),
//! because the iCalendar/MIME parsers arrive with the provider steps. Scopes are
//! JMAP `(account, type)` scopes (`Email` for mail, `CalendarEvent` for calendar);
//! the harness uses one fixed [`engine_store::ManualClock`] — lease expiry never
//! races in a single process.

mod cli;
mod ingest;

use std::path::Path;

use engine_core::calendar::Event;
use engine_core::ids::AccountId;
use engine_core::mail::Message;
use engine_core::sync::{JmapDataType, SyncScope};
use engine_search::{CalendarQuery, MailQuery, ParseError, SearchResults};
use engine_store::{Clock, ManualClock, StoreError};
use store_sqlite::SqliteStore;

pub use cli::{USAGE, run};
pub use engine_recurrence::Horizon;
pub use ingest::{IngestReport, ingest, reexpand_calendar};

/// The fixed instant the harness clock reports. A single-process CLI never races a
/// lease, so any stable instant works; the TTL keeps each claim live for the run.
const CLOCK_INSTANT: &str = "2026-01-01T00:00:00Z";

/// The worker identity stamped on the harness's leases.
pub(crate) const WORKER: &str = "engine-cli";

/// The cursor the harness advances scopes to (it does not sync a provider).
pub(crate) const CURSOR: &str = "engine-cli";

/// An error from a CLI pipeline step.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CliError {
    /// The store rejected an operation.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    /// Recurrence expansion failed for an event.
    #[error("recurrence expansion error: {0}")]
    Expand(#[from] engine_recurrence::ExpandError),
    /// A query string did not parse.
    #[error("query parse error: {0}")]
    Parse(#[from] ParseError),
    /// A fixture could not be read or deserialized.
    #[error("fixture error: {0}")]
    Fixture(String),
    /// The command-line invocation was invalid.
    #[error("usage error: {0}")]
    Usage(String),
}

/// A fixture of normalized objects to ingest.
///
/// Either list may be empty; each non-empty list is applied under its JMAP scope.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Fixture {
    /// Calendar events (projected and expanded into occurrences).
    #[serde(default)]
    pub events: Vec<Event>,
    /// Mail messages (projected; no occurrences).
    #[serde(default)]
    pub messages: Vec<Message>,
}

impl Fixture {
    /// Parses a fixture from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Fixture`] if the JSON is malformed or does not match the
    /// fixture shape.
    pub fn from_json(json: &str) -> Result<Self, CliError> {
        serde_json::from_str(json).map_err(|e| CliError::Fixture(e.to_string()))
    }
}

/// Opens (creating if absent) a file-backed store, migrated to the latest schema.
///
/// # Errors
///
/// Returns [`CliError::Store`] if the database cannot be opened or migrated.
pub fn open(path: impl AsRef<Path>) -> Result<SqliteStore<ManualClock>, CliError> {
    Ok(SqliteStore::open(path, clock())?)
}

/// Opens an ephemeral in-memory store (one connection = one database).
///
/// # Errors
///
/// Returns [`CliError::Store`] if the database cannot be opened or migrated.
pub fn open_in_memory() -> Result<SqliteStore<ManualClock>, CliError> {
    Ok(SqliteStore::open_in_memory(clock())?)
}

/// The harness clock: a fixed instant (see [`CLOCK_INSTANT`]).
fn clock() -> ManualClock {
    ManualClock::new(
        CLOCK_INSTANT
            .parse()
            .expect("CLOCK_INSTANT is a valid instant"),
    )
}

/// The JMAP mail scope for an account.
pub(crate) fn mail_scope(account: AccountId) -> SyncScope {
    SyncScope::JmapType {
        account,
        data_type: JmapDataType::Email,
    }
}

/// The JMAP calendar-event scope for an account.
pub(crate) fn calendar_scope(account: AccountId) -> SyncScope {
    SyncScope::JmapType {
        account,
        data_type: JmapDataType::CalendarEvent,
    }
}

/// Searches mail in `account`'s mail scope with a DSL query.
///
/// # Errors
///
/// Returns [`CliError::Parse`] for a malformed query or [`CliError::Store`] on a
/// backend failure.
pub async fn search_mail<C: Clock>(
    store: &SqliteStore<C>,
    account: AccountId,
    query: &str,
    limit: usize,
) -> Result<SearchResults, CliError> {
    let parsed = MailQuery::parse(query)?;
    let scope = mail_scope(account);
    Ok(store
        .search_mail(std::slice::from_ref(&scope), &parsed, limit)
        .await?)
}

/// Searches calendar events in `account`'s calendar scope with a DSL query.
///
/// Time-range (`before:`/`after:`) filters match materialized occurrences, so a
/// range answer reflects the current expansion horizon.
///
/// # Errors
///
/// Returns [`CliError::Parse`] for a malformed query or [`CliError::Store`] on a
/// backend failure.
pub async fn search_calendar<C: Clock>(
    store: &SqliteStore<C>,
    account: AccountId,
    query: &str,
    limit: usize,
) -> Result<SearchResults, CliError> {
    let parsed = CalendarQuery::parse(query)?;
    let scope = calendar_scope(account);
    Ok(store
        .search_calendar(std::slice::from_ref(&scope), &parsed, limit)
        .await?)
}
