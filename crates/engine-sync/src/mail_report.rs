//! What a mail pass reports: the per-folder outcome, its timing, and the account's roll-up.
//!
//! Split from [`mail_account`](crate::mail_account), which runs the pass, because these are
//! the *host-facing* half — every field here is read by a caller rendering a sync, and none
//! of it is read by the fan-out. They grew past the 500-line limit together.

use core::time::Duration;
use std::time::Instant;

use engine_core::sync::SyncScope;
use engine_store::SyncApplied;

use crate::SyncError;

/// Where a folder's sync spent its time.
///
/// **Reported rather than logged, because the engine is a library.** A log line is the host's
/// product surface — its wording, its level, its privacy rules — and a duration is a fact. The
/// same seam as `ConnectObserver`: the engine says what happened, the host decides how to say it.
///
/// The three do **not** sum to [`FolderSync::elapsed`]. What is left over is claiming and
/// releasing the scope lease, and the bookkeeping between chunks — small, and worth seeing as a
/// remainder rather than being folded into one of the buckets it is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncTiming {
    /// Awaiting the provider: the network, and whatever the adapter does to parse the wire.
    pub fetching: Duration,
    /// Projecting what arrived into rows — envelopes, the message-id graph, addresses.
    pub deriving: Duration,
    /// The store: the apply that commits a chunk, plus the recipient read it needs first.
    pub storing: Duration,
}

impl SyncTiming {
    /// Adds one phase's measurement to the running total.
    pub(crate) fn add_fetching(&mut self, at: Instant) {
        self.fetching += at.elapsed();
    }

    pub(crate) fn add_deriving(&mut self, at: Instant) {
        self.deriving += at.elapsed();
    }

    pub(crate) fn add_storing(&mut self, at: Instant) {
        self.storing += at.elapsed();
    }
}

/// One folder's outcome within an account pass.
#[derive(Debug)]
pub struct FolderSync {
    /// The folder's mail scope.
    pub scope: SyncScope,
    /// What it applied, or why it did not.
    pub result: Result<SyncApplied, SyncError>,
    /// Wall time for this folder alone. Folders overlap, so these do **not** sum to the pass.
    pub elapsed: Duration,
    /// Where that time went — network, projection, store.
    pub timing: SyncTiming,
}

/// What one account-level mail sync did.
///
/// **Returned whole, never behind a `Result`.** A partial failure is the ordinary case — one
/// folder busy, one refused, the rest fine — and collapsing that into a single `Err` throws away
/// exactly what a caller needs to tell an outage from an expired sign-in from a concurrent pass.
#[derive(Debug)]
pub struct MailSyncReport {
    /// The account-level **store** steps: the thread-index repair, the recipient backfill and the
    /// coverage record.
    ///
    /// An `Err` here is a store fault and never a network one, so a caller must not read it as
    /// the account being unreachable. That distinction is the whole reason it is its own field: a
    /// broken store that reports itself as an outage sends the user to check their wifi.
    pub account_steps: Result<(), SyncError>,
    /// The folder-list (container) sync, or `None` when this pass never looked at it — which is
    /// what [`refresh_folders`](crate::refresh_folders) does.
    ///
    /// `None` is not a success and not a failure: nothing was asked of the server, so a caller
    /// weighing whether the account is reachable must treat it the way it treats a scope another
    /// pass was holding, and read the folders instead.
    pub mailboxes: Option<Result<SyncApplied, SyncError>>,
    /// One entry per folder, in completion order.
    pub folders: Vec<FolderSync>,
    /// Wall time for the whole pass.
    pub elapsed: Duration,
}

impl MailSyncReport {
    /// Objects upserted across every folder that succeeded.
    #[must_use]
    pub fn upserted(&self) -> usize {
        self.folders
            .iter()
            .filter_map(|f| f.result.as_ref().ok())
            .map(|applied| applied.upserted)
            .sum()
    }

    /// Objects tombstoned across every folder that succeeded.
    #[must_use]
    pub fn tombstoned(&self) -> usize {
        self.folders
            .iter()
            .filter_map(|f| f.result.as_ref().ok())
            .map(|applied| applied.tombstoned)
            .sum()
    }

    /// Whether every scope this pass touched succeeded.
    ///
    /// A convenience for a caller that only needs "did this work" — anything deciding what to
    /// *tell the user* should read the fields, because "one folder was busy" and "the credential
    /// was refused" are different answers and this collapses them.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.account_steps.is_ok()
            && self.mailboxes.as_ref().is_none_or(Result::is_ok)
            && self.folders.iter().all(|f| f.result.is_ok())
    }

    /// The first failure this pass hit, in the order the steps ran.
    #[must_use]
    pub fn first_error(&self) -> Option<&SyncError> {
        self.account_steps
            .as_ref()
            .err()
            .or_else(|| self.mailboxes.as_ref().and_then(|r| r.as_ref().err()))
            .or_else(|| self.folders.iter().find_map(|f| f.result.as_ref().err()))
    }

    /// How many of this pass's scopes were skipped because another pass held them.
    #[must_use]
    pub fn busy_scopes(&self) -> usize {
        usize::from(
            self.mailboxes
                .as_ref()
                .is_some_and(|r| r.as_ref().is_err_and(SyncError::is_busy)),
        ) + self
            .folders
            .iter()
            .filter(|f| f.result.as_ref().is_err_and(SyncError::is_busy))
            .count()
    }

    /// How many folders this pass actually synced.
    #[must_use]
    pub fn folders_synced(&self) -> usize {
        self.folders.iter().filter(|f| f.result.is_ok()).count()
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use engine_core::{ids::AccountId, sync::SyncScope};
    use engine_store::SyncApplied;

    use super::{FolderSync, MailSyncReport, SyncTiming};
    use crate::SyncError;

    fn scope(mailbox: &str) -> SyncScope {
        SyncScope::ImapMailbox {
            account: AccountId::try_from("acct").unwrap(),
            mailbox: engine_core::ids::MailboxId::try_from(mailbox).unwrap(),
        }
    }

    fn folder(mailbox: &str, result: Result<SyncApplied, SyncError>) -> FolderSync {
        FolderSync {
            scope: scope(mailbox),
            result,
            elapsed: Duration::ZERO,
            timing: SyncTiming::default(),
        }
    }

    fn report(
        account_steps: Result<(), SyncError>,
        mailboxes: Option<Result<SyncApplied, SyncError>>,
        folders: Vec<FolderSync>,
    ) -> MailSyncReport {
        MailSyncReport {
            account_steps,
            mailboxes,
            folders,
            elapsed: Duration::ZERO,
        }
    }

    /// The order matters: a caller showing one message shows the earliest thing that broke,
    /// and a folder failure reported ahead of the account step that caused it reads as an
    /// unrelated outage.
    #[test]
    fn the_first_error_is_the_earliest_step_that_failed() {
        let account_failed = report(
            Err(SyncError::Decode("account step".to_owned())),
            Some(Err(SyncError::Decode("folder list".to_owned()))),
            vec![folder("INBOX", Err(SyncError::Decode("inbox".to_owned())))],
        );
        assert_eq!(
            account_failed.first_error().map(ToString::to_string),
            Some("decode error: account step".to_owned()),
        );

        let list_failed = report(
            Ok(()),
            Some(Err(SyncError::Decode("folder list".to_owned()))),
            vec![folder("INBOX", Err(SyncError::Decode("inbox".to_owned())))],
        );
        assert_eq!(
            list_failed.first_error().map(ToString::to_string),
            Some("decode error: folder list".to_owned()),
        );

        let only_a_folder_failed = report(
            Ok(()),
            Some(Ok(SyncApplied::default())),
            vec![
                folder("INBOX", Ok(SyncApplied::default())),
                folder("Archive", Err(SyncError::Decode("archive".to_owned()))),
            ],
        );
        assert_eq!(
            only_a_folder_failed.first_error().map(ToString::to_string),
            Some("decode error: archive".to_owned()),
        );
    }

    /// A refresh that looked at no folder list is not a failure — `None` is "not asked".
    #[test]
    fn a_pass_with_nothing_wrong_has_no_first_error() {
        let clean = report(
            Ok(()),
            None,
            vec![folder("INBOX", Ok(SyncApplied::default()))],
        );
        assert!(clean.first_error().is_none());
        assert!(clean.is_ok());
        assert_eq!(clean.folders_synced(), 1);
    }
}
