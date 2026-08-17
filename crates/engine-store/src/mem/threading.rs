//! The reference implementation of incremental thread maintenance.
//!
//! Mirrors `store-sqlite`'s `derived_ops::threading` step for step, in memory and without its
//! indices — the shared contract suite is what holds the two to the same answer. It is written
//! **incrementally**, not as a recompute, on purpose: a full re-derivation would also split a
//! component a deletion disconnected and rename one whose only owner was deleted, and the real
//! backend deliberately leaves both to a rebuild. A reference store that quietly did more would
//! make the contract suite pass on a behaviour only one backend has.
//!
//! It runs across the **account**, not the scope: a reply filed in Sent and its original in the
//! Inbox are distinct objects in distinct scopes and one conversation, so a per-scope pass could
//! never join them.

use std::collections::HashMap;

use engine_core::{
    ids::{AccountId, ProviderKey, ThreadId},
    search_index::{MailRefRow, MailRow},
    sync::SyncScope,
};

use super::ScopeCell;

/// One message as the graph sees it: where it lives, what thread it is on, and the ids it carries.
struct Member {
    scope: SyncScope,
    key: ProviderKey,
    thread: Option<ThreadId>,
    ids: Vec<MailRefRow>,
}

/// Assigns a thread to every message in `messages` the provider did not thread, merging the
/// components its ids reach across the account.
pub(super) fn assign_threads(
    scopes: &mut HashMap<SyncScope, ScopeCell>,
    account: &AccountId,
    scope: &SyncScope,
    messages: &[MailRow],
    refs: &[MailRefRow],
) {
    let mut by_key: HashMap<&str, Vec<&MailRefRow>> = HashMap::new();
    for row in refs {
        by_key.entry(row.key.as_str()).or_default().push(row);
    }

    // Everything the account holds, read once — the reference store has no index to look a
    // component up through, so it walks.
    let stored = account_members(scopes, account);

    let mut assignments: Vec<(SyncScope, ProviderKey, ThreadId)> = Vec::new();
    let derivable: Vec<&MailRow> = messages
        .iter()
        .filter(|row| row.thread_id.is_none())
        .collect();

    let mut graphed: Vec<&MailRow> = Vec::new();
    for row in derivable {
        if by_key.contains_key(row.key.as_str()) {
            graphed.push(row);
        } else if let Ok(thread) = ThreadId::try_from(row.key.as_str())
            && scopes
                .get(scope)
                .and_then(|cell| cell.messages.get(&row.key))
                .is_some_and(|stored| stored.thread_id.is_none())
        {
            // No ids at all: nothing can ever share one with it, so it is a singleton named
            // after its own key. Written only when the row has no thread yet — a message re-sent
            // whole says nothing about its threading (matches `store-sqlite`).
            assignments.push((scope.clone(), row.key.clone(), thread));
        }
    }

    for component in components(&graphed, &by_key) {
        let ids: Vec<&str> = component
            .iter()
            .flat_map(|row| by_key[row.key.as_str()].iter().map(|r| r.msgid.as_str()))
            .collect();
        let reached: Vec<ThreadId> = stored
            .iter()
            .filter(|member| {
                member
                    .ids
                    .iter()
                    .any(|row| ids.contains(&row.msgid.as_str()))
            })
            .filter_map(|member| member.thread.clone())
            .collect();

        // The smallest owned id in the merged component — this batch's, plus every stored
        // member's that sits under a thread the component reached.
        let owned = component
            .iter()
            .flat_map(|row| by_key[row.key.as_str()].iter())
            .filter(|row| row.owned)
            .map(|row| row.msgid.as_str().to_owned())
            .chain(
                members_of(&stored, &reached)
                    .flat_map(|member| member.ids.iter())
                    .filter(|row| row.owned)
                    .map(|row| row.msgid.as_str().to_owned()),
            )
            .min();
        let smallest_key = component
            .iter()
            .map(|row| row.key.as_str().to_owned())
            .chain(members_of(&stored, &reached).map(|member| member.key.as_str().to_owned()))
            .min();
        let Some(thread) = owned
            .or(smallest_key)
            .and_then(|id| ThreadId::try_from(id.as_str()).ok())
        else {
            continue;
        };

        // Re-key every graphed member already stored under a thread this component reached. Only
        // graphed ones: a provider-threaded message's id is the provider's and must not be caught
        // by a string match against a derived one.
        for member in members_of(&stored, &reached) {
            assignments.push((member.scope.clone(), member.key.clone(), thread.clone()));
        }
        for row in component {
            assignments.push((scope.clone(), row.key.clone(), thread.clone()));
        }
    }

    for (scope, key, thread) in assignments {
        if let Some(cell) = scopes.get_mut(&scope)
            && let Some(row) = cell.messages.get_mut(&key)
        {
            row.thread_id = Some(thread);
        }
    }
}

/// Every graphed message the account holds. A message with no `msgid_ref` rows is not in the
/// graph, so nothing here can reach it.
fn account_members(scopes: &HashMap<SyncScope, ScopeCell>, account: &AccountId) -> Vec<Member> {
    let mut members = Vec::new();
    for (scope, cell) in scopes {
        if scope.account() != account {
            continue;
        }
        for (key, row) in &cell.messages {
            let Some(ids) = cell.refs.get(key) else {
                continue;
            };
            if ids.is_empty() {
                continue;
            }
            members.push(Member {
                scope: scope.clone(),
                key: key.clone(),
                thread: row.thread_id.clone(),
                ids: ids.clone(),
            });
        }
    }
    members
}

/// The stored members sitting under any of `threads`.
fn members_of<'a>(
    stored: &'a [Member],
    threads: &'a [ThreadId],
) -> impl Iterator<Item = &'a Member> {
    stored.iter().filter(move |member| {
        member
            .thread
            .as_ref()
            .is_some_and(|thread| threads.contains(thread))
    })
}

/// The batch's messages grouped by the ids they share **with each other**.
///
/// A cost fix, not a correctness one (see `store-sqlite`'s twin): one message at a time reaches
/// the same grouping, but re-keys the partial component on every step.
fn components<'a>(
    graphed: &[&'a MailRow],
    by_key: &HashMap<&str, Vec<&MailRefRow>>,
) -> Vec<Vec<&'a MailRow>> {
    let mut parent: Vec<usize> = (0..graphed.len()).collect();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, row) in graphed.iter().enumerate() {
        for id in by_key[row.key.as_str()].iter().map(|r| r.msgid.as_str()) {
            match seen.get(id) {
                Some(&other) => union(&mut parent, index, other),
                None => {
                    seen.insert(id, index);
                }
            }
        }
    }
    let mut grouped: HashMap<usize, Vec<&'a MailRow>> = HashMap::new();
    for (index, row) in graphed.iter().enumerate() {
        grouped
            .entry(find(&mut parent, index))
            .or_default()
            .push(row);
    }
    grouped.into_values().collect()
}

fn find(parent: &mut [usize], index: usize) -> usize {
    let mut root = index;
    while parent[root] != root {
        root = parent[root];
    }
    let mut node = index;
    while parent[node] != root {
        let next = parent[node];
        parent[node] = root;
        node = next;
    }
    root
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}
