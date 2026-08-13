//! The [`StoreRead`](crate::StoreRead) query path for `MemStore`: scope and
//! object reads, the mail list, op state, and index-row counts.

use std::{cmp::Reverse, collections::BTreeSet};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey, ThreadId},
    search_index::MembershipKind,
    sync::SyncScope,
    time::{ExpansionWindow, Horizon},
    write::PendingOpId,
};
use serde_json::Value;

use super::{MemStore, ScopeCell};
use crate::{
    apply::OccurrenceRow,
    error::Result,
    lease::Clock,
    outbox::PendingOpState,
    store::{IndexRowCounts, MailListRow, MailSelector, StoreRead},
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

    async fn expansion_window(&self, scope: &SyncScope) -> Result<Option<ExpansionWindow>> {
        Ok(self
            .lock()
            .scopes
            .get(scope)
            .and_then(|cell| cell.window.clone()))
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

    async fn list_mail(
        &self,
        accounts: &[AccountId],
        select: MailSelector<'_>,
        limit: usize,
    ) -> Result<Vec<MailListRow>> {
        let wanted: BTreeSet<&AccountId> = accounts.iter().collect();
        let inner = self.lock();
        // The reference store has no index to seek into, so it scans every scope of every named
        // account and orders in memory. What it pins is the *answer*; a backend that stores an
        // ordered index on the date column reads only the rows it returns.
        let mut rows: Vec<MailListRow> = Vec::new();
        for (scope, cell) in &inner.scopes {
            if !wanted.contains(scope.account()) {
                continue;
            }
            // Mail rows are cleared on tombstone (`remove_derived`), so the stored ones are
            // exactly the scope's live mail objects — no separate liveness join needed.
            for (key, mail) in &cell.messages {
                if !selects(select, key, mail.thread_id.as_ref()) {
                    continue;
                }
                rows.push(MailListRow {
                    account: scope.account().clone(),
                    mailboxes: mailboxes_of(cell, key),
                    mail: mail.clone(),
                });
            }
        }
        // Newest first, undated last, ties broken on the row's own identity so the sequence — and
        // so the window `limit` cuts — is the same on every read of an unchanged store.
        rows.sort_by(|a, b| {
            Reverse(a.mail.date_utc)
                .cmp(&Reverse(b.mail.date_utc))
                .then_with(|| (&a.account, &a.mail.key).cmp(&(&b.account, &b.mail.key)))
        });
        rows.truncate(limit);
        Ok(rows)
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
            message: usize::from(cell.messages.contains_key(key)),
            addresses: cell.addresses.get(key).map_or(0, Vec::len),
            memberships: cell.memberships.get(key).map_or(0, Vec::len),
            event_index: usize::from(cell.event_index.contains_key(key)),
            participants: cell.participants.get(key).map_or(0, Vec::len),
        })
    }
}

/// Whether one stored row is named by `select`. An empty `Threads`/`Keys` slice names nothing,
/// which is what makes "complete these conversations" with no conversations a no-op.
fn selects(select: MailSelector<'_>, key: &ProviderKey, thread: Option<&ThreadId>) -> bool {
    match select {
        MailSelector::Newest => true,
        MailSelector::Threads(threads) => thread.is_some_and(|id| threads.contains(id)),
        MailSelector::Keys(keys) => keys.contains(key),
    }
}

/// One message's mailbox membership, out of the junction rows that hold every axis.
fn mailboxes_of(cell: &ScopeCell, key: &ProviderKey) -> Vec<MailboxId> {
    cell.memberships
        .get(key)
        .into_iter()
        .flatten()
        .filter(|row| row.kind == MembershipKind::Mailbox)
        .filter_map(|row| MailboxId::try_from(row.value.as_str()).ok())
        .collect()
}
