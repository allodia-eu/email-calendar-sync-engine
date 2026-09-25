//! Drainer tests: the pass that attempts what the inline drivers left behind.
//!
//! The case these exist for is the one issue #60 opens with: a write attempted with no
//! network, which used to be recorded and then forgotten. Split by what the pass is
//! deciding: [`sends`] covers a submission's outcomes (the one write that must never be
//! repeated), [`other_writes`] the edits and reports and everything a pass leaves alone.

mod other_writes;
mod sends;

use super::*;

/// The message a queued edit targets.
fn target() -> ProviderKey {
    ProviderKey::new("imap:v1:u42@INBOX").unwrap()
}

/// A store plus the handle that moves its clock: `ManualClock` shares one instant, so
/// the copy held here advances the one the store reads.
pub(super) fn store_and_clock() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = clock();
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

/// Queues a send by letting one inline submission fail against `provider`.
async fn queue_a_send(provider: &FakeMail, store: &SqliteStore<ManualClock>, id: &str) {
    submit_mail(
        provider,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft(id),
    )
    .await
    .expect_err("this submission is meant to fail and leave the send queued");
}
