//! Forgetting one domain of an account (its mail, its calendars or its contacts) while the
//! other two keep their objects, scopes and cursors.
//!
//! A host calls this when one kind of data stops being synced for an account, so that
//! syncing it again later starts clean, as [`forget_account`](crate::SqliteStore::forget_account)
//! does for a whole account. Like that call it is not lease-gated: the host stops the domain's
//! syncs first, and the store's single connection serialises the purge against anything else.

use engine_core::{
    ids::AccountId,
    sync::{SearchDomain, SyncScope},
    write::PendingOpKind,
};
use engine_store::{Clock, Result};
use rusqlite::Connection;

use super::SCOPE_TABLES;
use crate::{
    SqliteStore,
    convert::{backend, kind_to_text},
    sql,
};

/// Account-keyed tables whose every row is mail: the cached bodies and raw sources, and the
/// recipient index's coverage and backfill marker. The observations themselves are handled
/// apart, because a suppressed one outlives the mail it came from.
pub(super) const MAIL_TABLES: &[&str] = &[
    "message_source",
    "message_body",
    "recipient_coverage",
    "recipient_index_state",
    "server_keywords",
];

/// Account-keyed tables whose every row is contacts.
pub(super) const CONTACT_TABLES: &[&str] = &["contact_photo"];

impl<C: Clock> SqliteStore<C> {
    /// Purges every durable trace of one `domain` of `account`, in one transaction: the
    /// domain's sync scopes with their cursors, objects and derived rows, its queued writes,
    /// and the caches only that domain fills. The account's other domains, and every other
    /// account, are untouched.
    ///
    /// - **Mail** also drops the cached bodies and raw sources and the recipient history derived
    ///   from sent mail, except an observation the user asked to forget: it stays suppressed, so
    ///   the same mail synced again cannot bring the address back.
    /// - **Contacts** also drops the cached photos and moves the contact generation, so the people
    ///   index reads as stale until it is rebuilt from the sources that remain.
    ///
    /// A scope or queued write whose domain this build cannot tell (a JMAP data type it does
    /// not know, an op enqueued before the store recorded kinds) is kept; forgetting the
    /// account removes it. Blobs are reclaimed by
    /// [`sweep_unreferenced_blobs`](Self::sweep_unreferenced_blobs), as after
    /// [`forget_account`](Self::forget_account).
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn forget_account_domain(
        &self,
        account: &AccountId,
        domain: SearchDomain,
    ) -> Result<()> {
        let account = account.as_str().to_owned();
        self.call(move |conn| purge_domain(conn, &account, domain))
            .await
    }
}

/// Deletes `domain`'s rows for `account` across every table in one transaction, each
/// scope's `sync_scope` row after its scope-keyed rows.
pub(super) fn purge_domain(
    conn: &mut Connection,
    account: &str,
    domain: SearchDomain,
) -> Result<()> {
    let tx = conn.transaction().map_err(backend)?;
    let keys = sql::query_all(
        &tx,
        "SELECT scope_key FROM sync_scope WHERE account = ?1",
        [account],
        |r| r.get::<_, String>(0),
    )?;
    // The key is the scope's own serialisation, so it names its domain; a key this build
    // cannot read back names none and is kept.
    let in_domain = keys.into_iter().filter(|key| {
        serde_json::from_str::<SyncScope>(key)
            .ok()
            .and_then(|scope| scope.domain())
            == Some(domain)
    });
    for key in in_domain {
        for table in SCOPE_TABLES.iter().chain(&["sync_scope"]) {
            tx.execute(&format!("DELETE FROM {table} WHERE scope_key = ?1"), [&key])
                .map_err(backend)?;
        }
    }
    for kind in PendingOpKind::ALL {
        if kind.domain() == domain {
            tx.execute(
                "DELETE FROM pending_op WHERE account = ?1 AND kind = ?2",
                (account, kind_to_text(kind)),
            )
            .map_err(backend)?;
        }
    }
    let tables = match domain {
        SearchDomain::Mail => {
            tx.execute(
                "DELETE FROM recipient_observation WHERE account = ?1 AND suppressed = 0",
                [account],
            )
            .map_err(backend)?;
            MAIL_TABLES
        }
        SearchDomain::Contacts => {
            tx.execute(
                "UPDATE contact_state SET generation = generation + 1 WHERE singleton = 1",
                [],
            )
            .map_err(backend)?;
            CONTACT_TABLES
        }
        SearchDomain::Calendar => &[],
    };
    for table in tables {
        tx.execute(
            &format!("DELETE FROM {table} WHERE account = ?1"),
            [account],
        )
        .map_err(backend)?;
    }
    tx.commit().map_err(backend)?;
    Ok(())
}

#[cfg(test)]
#[path = "domain_tests.rs"]
mod tests;
