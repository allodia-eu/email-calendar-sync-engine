//! Getting a generated mailbox into an [`Engine`].

use engine_api::{ApiError, Engine, IgnoreCommits, MailSyncReport, StreamTuning};

use crate::{
    generate::{Fixture, generate},
    provider::{FolderProvider, Pass},
    spec::FixtureSpec,
};

/// Generates the mailbox `spec` describes and syncs it into `engine`.
///
/// The mail arrives through the streaming sync path a host drives — the folder list
/// once, then each folder's mail as reconciling pages — so the store ends up in the
/// state a real first sync leaves it in, including every derived search and index row.
///
/// The generated messages already carry the thread ids derivation would assign (see
/// [`crate::generate`]), so no derivation pass runs here. A caller that wants to
/// *measure* derivation calls [`Engine::rebuild_thread_index`] itself; over a fixture it
/// is the steady-state pass — a full scan that writes nothing.
///
/// # Errors
///
/// Returns the [`ApiError`] the underlying sync reports.
pub async fn populate(engine: &Engine, spec: &FixtureSpec) -> Result<Fixture, ApiError> {
    let fixture = generate(spec);
    let mailboxes = fixture.mailboxes();
    // One provider per folder, handed over together: the engine syncs the shared folder-list
    // scope once and fans the folders out itself, which is the path a host drives.
    let providers: Vec<FolderProvider> = fixture
        .folders
        .iter()
        .map(|folder| {
            FolderProvider::new(
                folder.id.clone(),
                mailboxes.clone(),
                Pass::Snapshot(folder.messages.clone()),
            )
        })
        .collect();
    engine
        .sync_mail(
            &providers,
            &spec.account,
            // The provider decides its own paging (`POPULATE_CHUNK`), so neither knob applies
            // here; both are passed at their "provider's choice" value.
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await;
    Ok(fixture)
}

/// Syncs one `pass` into the fixture's folder at `index`, through the same streaming
/// path a host drives.
///
/// The write half of the fixture: a [`Pass::Delta`] carrying one message with a
/// changed keyword is a mark-read, and one carrying a page of messages is an
/// incremental sync.
///
/// # Errors
///
/// Returns the [`ApiError`] the underlying sync reports.
///
/// # Panics
///
/// Panics if `index` is not a folder of `fixture`.
pub async fn sync_folder(
    engine: &Engine,
    spec: &FixtureSpec,
    fixture: &Fixture,
    index: usize,
    pass: Pass,
) -> MailSyncReport {
    let folder = &fixture.folders[index];
    let provider = FolderProvider::new(folder.id.clone(), fixture.mailboxes(), pass);
    // A whole account pass over a one-folder account — the shape a JMAP, Gmail or Graph account
    // actually has. It therefore includes the folder-list scope and the account-level store steps,
    // which the old per-folder entrypoint did not: a delta timing from here is a whole pass, and
    // is not comparable with one taken before the entrypoints were folded together.
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &spec.account,
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await
}
