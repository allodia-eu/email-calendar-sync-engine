//! `mailbox-fixture` — a synthetic mailbox at real scale, and the table that reports
//! what it costs.
//!
//! Every claim about engine performance is a claim about a mailbox, and the mailboxes
//! the test suite builds hold single digits of messages. This crate builds one that
//! holds hundreds of thousands: deterministic, conversation-shaped, and pushed into
//! the store through the same claim → project → apply → release cycle a real sync
//! uses, so a number measured against it is a number about the code that ships.
//!
//! ```no_run
//! # async fn run() -> Result<(), engine_api::ApiError> {
//! use engine_api::{AccountId, Engine};
//! use mailbox_fixture::{FixtureSpec, populate};
//!
//! let account = AccountId::try_from("acct-1").unwrap();
//! let engine = Engine::open("/tmp/scale.sqlite")?;
//! let fixture = populate(&engine, &FixtureSpec::new(account.clone(), 100_000)).await?;
//!
//! let first_page = engine.mail_window(&[account], 100).await?;
//! assert_eq!(first_page.len(), 100);
//! assert_eq!(fixture.len(), 100_000);
//! # Ok(())
//! # }
//! ```
//!
//! The three pieces:
//!
//! - [`generate`] builds the mailbox: conversations with real reference graphs, filed across six
//!   folders, dated over a fixed five-year window. Deterministic in its seed, so a baseline
//!   captured today is comparable to one captured after a refactor.
//! - [`populate`] and [`sync_folder`] put it into an [`Engine`](engine_api::Engine) through
//!   [`FolderProvider`], an offline provider bound to one folder — IMAP-shaped, so each folder is
//!   its own sync scope exactly as a real account's is.
//! - [`Recorder`] reduces raw timings to `n / p50 / p90 / p99 / max`, the same shape a host reduces
//!   its own logged durations to, so the two tables can be read together.
//!
//! Benchmarks over this fixture live in `benches/`; `SCALE` below is how they are
//! pointed at a size.

mod generate;
mod populate;
mod provider;
mod report;
mod rng;
mod scale;
mod spec;
mod words;

pub use generate::{Fixture, Folder, generate};
pub use populate::{populate, sync_folder};
pub use provider::{FolderProvider, POPULATE_CHUNK, Pass};
pub use report::Recorder;
pub use scale::{SCALE, Scale};
pub use spec::FixtureSpec;
