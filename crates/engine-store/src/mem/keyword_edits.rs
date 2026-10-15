//! Showing a queued keyword change before the server confirms it: `MemStore`'s side of
//! [`KeywordEdit`]. The same three moments as `store-sqlite`'s `keyword_edits`, which carries the
//! reasoning; the contract suite holds the two to one behaviour.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, ProviderKey},
    mail::{Keyword, MailFlags},
    search_index::{MembershipKind, MembershipRow},
    time::UtcDateTime,
    write::KeywordEdit,
};

use super::{Inner, ScopeCell};
use crate::outbox::PendingOpState;

impl Inner {
    /// Shows `edit`, just queued by one of `account`'s ops.
    pub(super) fn keyword_edit_queued(&mut self, account: &AccountId, edit: &KeywordEdit) {
        let Some(held) = self.held_keywords(account, &edit.key) else {
            return;
        };
        let server = self
            .server_keywords
            .entry((account.clone(), edit.key.clone()))
            .or_insert(held)
            .clone();
        let unsettled = self.unsettled_edits(account, &edit.key);
        self.show(account, &edit.key, &KeywordEdit::shown(&server, &unsettled));
    }

    /// Re-applies the queued changes to `keys`, just written by a sync of `scope`'s rows.
    pub(super) fn keyword_edits_synced(
        &mut self,
        account: &AccountId,
        scope: &engine_core::sync::SyncScope,
        keys: &[ProviderKey],
        observed_from: Option<UtcDateTime>,
    ) {
        for key in keys {
            let Some(cell) = self.scopes.get(scope) else {
                return;
            };
            if !cell.messages.contains_key(key) {
                continue;
            }
            let server = row_keywords(cell, key);
            let accepted = observed_from.map_or_else(Vec::new, |since| {
                self.edits(account, key, |op| {
                    op.state == PendingOpState::Succeeded
                        && op.settled_at.is_some_and(|at| at > since)
                })
            });
            let unsettled = self.unsettled_edits(account, key);
            if unsettled.is_empty() && accepted.is_empty() {
                continue;
            }
            let server = KeywordEdit::shown(&server, &accepted);
            if !unsettled.is_empty() {
                self.server_keywords
                    .insert((account.clone(), key.clone()), server.clone());
            }
            self.show(account, key, &KeywordEdit::shown(&server, &unsettled));
        }
    }

    /// Applies every keyword-carrying op that reached a terminal state since the last call:
    /// an accepted change joins the server set, any other drops out of what is shown.
    ///
    /// One sweep after every outbox transition, rather than a call at each of the paths that
    /// can settle an op (an outcome, a cancellation, a recovered attempt whose retries ran out).
    pub(super) fn settle_keyword_edits(&mut self, now: UtcDateTime) {
        let newly: Vec<(AccountId, KeywordEdit, PendingOpState)> = self
            .ops
            .values_mut()
            .filter(|op| op.state.is_terminal() && op.settled_at.is_none())
            .filter_map(|op| {
                op.settled_at = Some(now);
                op.op
                    .keyword_edit
                    .clone()
                    .map(|edit| (op.account.clone(), edit, op.state))
            })
            .collect();
        for (account, edit, state) in newly {
            let entry = (account.clone(), edit.key.clone());
            let Some(mut server) = self.server_keywords.get(&entry).cloned() else {
                continue;
            };
            if state == PendingOpState::Succeeded {
                edit.apply_to(&mut server);
            }
            let unsettled = self.unsettled_edits(&account, &edit.key);
            if unsettled.is_empty() {
                self.server_keywords.remove(&entry);
            } else {
                self.server_keywords.insert(entry, server.clone());
            }
            self.show(
                &account,
                &edit.key,
                &KeywordEdit::shown(&server, &unsettled),
            );
        }
    }

    fn unsettled_edits(&self, account: &AccountId, key: &ProviderKey) -> Vec<KeywordEdit> {
        self.edits(account, key, |op| !op.state.is_terminal())
    }

    /// `account`'s changes to `key` whose op passes `keep`, in the order they were queued.
    fn edits(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        keep: impl Fn(&super::OpCell) -> bool,
    ) -> Vec<KeywordEdit> {
        self.ops
            .values()
            .filter(|op| op.account == *account && keep(op))
            .filter_map(|op| op.op.keyword_edit.clone())
            .filter(|edit| edit.key == *key)
            .collect()
    }

    fn held_keywords(&self, account: &AccountId, key: &ProviderKey) -> Option<BTreeSet<Keyword>> {
        self.scopes
            .iter()
            .find(|(scope, cell)| scope.account() == account && cell.messages.contains_key(key))
            .map(|(_, cell)| row_keywords(cell, key))
    }

    /// Writes `keywords` as what every one of `account`'s rows for `key` shows.
    fn show(&mut self, account: &AccountId, key: &ProviderKey, keywords: &BTreeSet<Keyword>) {
        let flags = MailFlags::from_keywords(keywords);
        for (scope, cell) in &mut self.scopes {
            if scope.account() != account {
                continue;
            }
            let Some(row) = cell.messages.get_mut(key) else {
                continue;
            };
            row.flags = flags;
            let memberships = cell.memberships.entry(key.clone()).or_default();
            memberships.retain(|m| m.kind != MembershipKind::Keyword);
            memberships.extend(keywords.iter().map(|keyword| MembershipRow {
                key: key.clone(),
                kind: MembershipKind::Keyword,
                value: keyword.as_str().to_owned(),
            }));
        }
    }
}

fn row_keywords(cell: &ScopeCell, key: &ProviderKey) -> BTreeSet<Keyword> {
    cell.memberships
        .get(key)
        .into_iter()
        .flatten()
        .filter(|m| m.kind == MembershipKind::Keyword)
        .filter_map(|m| Keyword::try_from(m.value.clone()).ok())
        .collect()
}
