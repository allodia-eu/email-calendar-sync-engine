//! The [`StoreRead`] query path for [`SqliteStore`]: scope and object reads, the mail
//! list, the calendar occurrence range read, op state, and index-row counts.
//!
//! Split from the writer/lease/outbox half in `lib.rs` (which is at the 500-line limit),
//! mirroring how the in-memory reference store separates `mem/read.rs` from `mem/write.rs`.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::SyncScope,
    time::{ExpansionWindow, Horizon},
    write::PendingOpId,
};
use engine_store::{
    Clock, IndexRowCounts, MailListRow, MailSelector, OccurrenceRow, PendingOpState, Result,
    StoreRead,
};
use serde_json::Value;

use crate::{SqliteStore, convert::scope_key, derived_ops, mail_ops, outbox_ops, scope_ops};

#[async_trait]
impl<C: Clock> StoreRead for SqliteStore<C> {
    async fn account_scopes(&self, account: AccountId) -> Result<Vec<SyncScope>> {
        self.read(move |conn| scope_ops::account_scopes(conn, &account))
            .await
    }

    async fn expansion_window(&self, scope: &SyncScope) -> Result<Option<ExpansionWindow>> {
        let key = scope_key(scope);
        self.read(move |conn| crate::window_ops::expansion_window(conn, &key))
            .await
    }

    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>> {
        let key = scope_key(scope);
        self.read(move |conn| scope_ops::object_keys(conn, &key))
            .await
    }

    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>> {
        let scope = scope_key(scope);
        let provider_key = key.as_str().to_owned();
        self.read(move |conn| scope_ops::object_payload(conn, &scope, &provider_key))
            .await
    }

    async fn scope_objects(&self, scope: &SyncScope) -> Result<Vec<(ProviderKey, Value)>> {
        let key = scope_key(scope);
        self.read(move |conn| scope_ops::scope_objects(conn, &key))
            .await
    }

    async fn list_mail(
        &self,
        accounts: &[AccountId],
        select: MailSelector<'_>,
        limit: usize,
    ) -> Result<Vec<MailListRow>> {
        let Some(select) = mail_ops::own(select) else {
            // An empty thread or key list names nothing, so the read is skipped rather than
            // compiled into a statement that cannot match.
            return Ok(Vec::new());
        };
        let accounts = accounts.to_vec();
        self.read(move |conn| mail_ops::list_mail(conn, &accounts, &select, limit))
            .await
    }

    async fn scope_occurrences(
        &self,
        scope: &SyncScope,
        window: Horizon,
    ) -> Result<Vec<OccurrenceRow>> {
        let key = scope_key(scope);
        self.read(move |conn| derived_ops::scope_occurrences(conn, &key, window))
            .await
    }

    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>> {
        self.read(move |conn| outbox_ops::pending_op_state(conn, id))
            .await
    }

    async fn index_row_counts(
        &self,
        scope: &SyncScope,
        key: &ProviderKey,
    ) -> Result<IndexRowCounts> {
        let scope = scope_key(scope);
        let provider_key = key.as_str().to_owned();
        self.read(move |conn| derived_ops::index_row_counts(conn, &scope, &provider_key))
            .await
    }
}
