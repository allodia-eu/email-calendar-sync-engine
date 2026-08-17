//! Getting a generated mailbox into an [`Engine`].

use engine_api::{ApiError, Engine, IgnoreCommits, StreamTuning, SyncApplied};

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
    // The folder-list scope is shared across every folder provider, so it syncs once.
    let first = FolderProvider::new(
        fixture.folders[0].id.clone(),
        mailboxes.clone(),
        Pass::Snapshot(Vec::new()),
    );
    engine.sync_mailbox_list(&first, &spec.account).await?;

    for (index, folder) in fixture.folders.iter().enumerate() {
        sync_folder(
            engine,
            spec,
            &fixture,
            index,
            Pass::Snapshot(folder.messages.clone()),
        )
        .await?;
    }
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
) -> Result<SyncApplied, ApiError> {
    let folder = &fixture.folders[index];
    let provider = FolderProvider::new(folder.id.clone(), fixture.mailboxes(), pass);
    engine
        .sync_folder_email_streamed(
            &provider,
            &spec.account,
            // The provider decides its own paging (`POPULATE_CHUNK`), so neither knob
            // applies here; both are passed at their "provider's choice" value.
            StreamTuning::new(0, 0),
            &IgnoreCommits,
        )
        .await
}
