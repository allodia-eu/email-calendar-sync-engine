//! The [`StoreRead`](crate::StoreRead) query path for `MemStore`: scope and
//! object reads, the mail index, op state, and index-row counts.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::SyncScope,
    time::Horizon,
    write::PendingOpId,
};
use serde_json::Value;

use super::MemStore;
use crate::{
    apply::OccurrenceRow,
    error::Result,
    lease::Clock,
    outbox::PendingOpState,
    store::{IndexRowCounts, MailIndexEntry, StoreRead},
};

#[async_trait]
impl<C: Clock> StoreRead for MemStore<C> {
    async fn account_scopes(&self, account: AccountId) -> Result<Vec<SyncScope>> {
        let inner = self.lock();
        let mut scopes: Vec<SyncScope> = inner
            .scopes
            .keys()
            .filter(|scope| scope.account() == &account)
            .cloned()
            .collect();
        scopes.sort();
        Ok(scopes)
    }

    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>> {
        let inner = self.lock();
        let mut keys: Vec<ProviderKey> = inner
            .scopes
            .get(scope)
            .map(|c| c.objects.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        Ok(keys)
    }

    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>> {
        let inner = self.lock();
        Ok(inner
            .scopes
            .get(scope)
            .and_then(|c| c.objects.get(key).cloned()))
    }

    async fn scope_objects(&self, scope: &SyncScope) -> Result<Vec<(ProviderKey, Value)>> {
        let inner = self.lock();
        let mut objects: Vec<(ProviderKey, Value)> = inner
            .scopes
            .get(scope)
            .map(|c| {
                c.objects
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        objects.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(objects)
    }

    async fn scope_mail_index(&self, scope: &SyncScope) -> Result<Vec<MailIndexEntry>> {
        let inner = self.lock();
        // The mail index is cleared on tombstone (`remove_derived`), so its entries are
        // exactly the scope's live mail objects — no separate liveness join needed.
        Ok(inner
            .scopes
            .get(scope)
            .map(|c| {
                c.mail_index
                    .iter()
                    .map(|(key, row)| (key.clone(), row.date_utc, row.thread_id.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn scope_occurrences(
        &self,
        scope: &SyncScope,
        window: Horizon,
    ) -> Result<Vec<OccurrenceRow>> {
        let inner = self.lock();
        // Occurrences are cleared on tombstone (`remove_derived`), so the stored rows are
        // exactly the live events' — no liveness join needed, as for the mail index.
        let Some(cell) = inner.scopes.get(scope) else {
            return Ok(Vec::new());
        };
        let mut rows: Vec<OccurrenceRow> = cell
            .occurrences
            .values()
            .flatten()
            .filter(|row| window.overlaps(row.start, row.end))
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            (a.start, a.end, &a.event, a.recurrence_id).cmp(&(
                b.start,
                b.end,
                &b.event,
                b.recurrence_id,
            ))
        });
        Ok(rows)
    }

    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>> {
        Ok(self.lock().ops.get(&id).map(|o| o.state))
    }

    async fn index_row_counts(
        &self,
        scope: &SyncScope,
        key: &ProviderKey,
    ) -> Result<IndexRowCounts> {
        let inner = self.lock();
        let Some(cell) = inner.scopes.get(scope) else {
            return Ok(IndexRowCounts::default());
        };
        Ok(IndexRowCounts {
            fts: usize::from(cell.fts.contains_key(key)),
            occurrences: cell.occurrences.get(key).map_or(0, Vec::len),
            mail_index: usize::from(cell.mail_index.contains_key(key)),
            addresses: cell.addresses.get(key).map_or(0, Vec::len),
            memberships: cell.memberships.get(key).map_or(0, Vec::len),
            event_index: usize::from(cell.event_index.contains_key(key)),
            participants: cell.participants.get(key).map_or(0, Vec::len),
        })
    }
}
