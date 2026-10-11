//! Whole-scope JMAP sync: every type a provider hands over as one [`ScopeSync`], however large.
//!
//! Two shapes cover every type: **containers** (`Mailbox`, `Calendar`, `AddressBook`) snapshot
//! with one `Foo/get`, since a server holds few of them; **members** (`CalendarEvent`,
//! `ContactCard`) snapshot by paging `Foo/query`, each page's ids fetched by a `#ids`
//! back-reference in the same request. Both delta by paging `Foo/changes`. Every page asks for at
//! most `maxObjectsInGet` ids (RFC 8620 §5.1), because a server refuses a larger `/get` outright
//! with `requestTooLarge` rather than truncating it; a scope larger than one `/get` therefore
//! takes several requests, and the rules below keep the result correct across them.
//!
//! - **The cursor is the state read with the first page.** A change landing mid-pass is then
//!   replayed by the next delta rather than lost behind a later state.
//! - **A snapshot whose query moved mid-pass tombstones nothing.** A `queryState` that differs
//!   between pages means an id may have shifted past a page boundary unseen, so the pass is applied
//!   as an additive delta, and the next delta reconciles what it missed.
//! - **Paged changes are applied in order,** so the last page to mention an object decides whether
//!   it exists.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    error::FailureClass,
    ids::ProviderKey,
    sync::{Keyed, SyncObject, SyncState, SyncUpdate},
};
use engine_provider::ScopeSync;
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    fetch::{MemberFetch, back_ref_get, query_page},
    request::Request,
    sync_ops::{Changes, is_complete, objects, snapshot_or_delta, state},
};

/// The most ids one page of a pass asks for: the server's `maxObjectsInGet`.
fn page_size(executor: &dyn Executor) -> usize {
    executor.session().limits().max_objects_in_get.max(1)
}

pub(crate) async fn container_sync<T: SyncObject>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    cursor: Option<&SyncState>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    key_of: impl Fn(&T) -> ProviderKey + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    container_sync_with_status(
        executor, account, using, type_name, cursor, normalize, key_of,
    )
    .await
    .map(|(sync, _)| sync)
}

/// Container sync plus whether an expired changes state forced a snapshot.
pub(crate) async fn container_sync_with_status<T: SyncObject>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    cursor: Option<&SyncState>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    key_of: impl Fn(&T) -> ProviderKey + Copy,
) -> Result<(ScopeSync<T>, bool), JmapError> {
    let fetch = MemberFetch {
        executor,
        account,
        using,
        type_name,
        properties: None,
    };
    let Some(cursor) = cursor else {
        return container_snapshot(&fetch, normalize, key_of)
            .await
            .map(|sync| (sync, false));
    };
    match paged_delta(&fetch, cursor, normalize).await {
        Err(e) if e.failure_class() == FailureClass::NeedsResync => {
            container_snapshot(&fetch, normalize, key_of)
                .await
                .map(|sync| (sync, true))
        }
        Ok(sync) => Ok((sync, false)),
        Err(error) => Err(error),
    }
}

async fn container_snapshot<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
    key_of: impl Fn(&T) -> ProviderKey,
) -> Result<ScopeSync<T>, JmapError> {
    let mut req = Request::new(fetch.using.iter().copied());
    let get = req.invoke(
        format!("{}/get", fetch.type_name),
        json!({ "accountId": fetch.account }),
    );
    let resp = fetch.executor.execute(&req).await?;
    let result = resp.result(&get)?;
    let items = objects(result, normalize)?;
    let present: BTreeSet<ProviderKey> = items.iter().map(key_of).collect();
    Ok(ScopeSync::new(
        SyncUpdate::snapshot(items, present),
        state(result, "state")?,
    ))
}

/// Syncs a member type, recovering to a snapshot on `cannotCalculateChanges`; also says
/// whether that recovery happened.
pub(crate) async fn member_sync_with_status<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    cursor: Option<&SyncState>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<(ScopeSync<T>, bool), JmapError> {
    let Some(cursor) = cursor else {
        return member_snapshot(fetch, normalize)
            .await
            .map(|sync| (sync, false));
    };
    match paged_delta(fetch, cursor, normalize).await {
        Err(e) if e.failure_class() == FailureClass::NeedsResync => {
            member_snapshot(fetch, normalize)
                .await
                .map(|sync| (sync, true))
        }
        Ok(sync) => Ok((sync, false)),
        Err(error) => Err(error),
    }
}

/// [`member_sync_with_status`] without the status.
pub(crate) async fn member_sync<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    cursor: Option<&SyncState>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    member_sync_with_status(fetch, cursor, normalize)
        .await
        .map(|(sync, _)| sync)
}

async fn member_snapshot<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    let limit = page_size(fetch.executor);
    let first = query_page(fetch, None, 0, limit, None, normalize).await?;
    let (cursor, query_state) = (first.state, first.query_state);
    let mut steady = true;
    let mut total = first.total;
    let mut present: BTreeSet<ProviderKey> = first.present.into_iter().collect();
    let mut fetched = Fetched::default();
    fetched.upsert(first.changed);
    let mut next = first.next_position;
    while let Some(position) = next {
        let page = query_page(fetch, None, position, limit, None, normalize).await?;
        steady &= page.query_state == query_state;
        total = page.total.or(total);
        present.extend(page.present);
        fetched.upsert(page.changed);
        next = page.next_position;
    }
    let complete = steady && is_complete(total, present.len());
    Ok(ScopeSync::new(
        snapshot_or_delta(fetched.into_changed(), present, complete),
        cursor,
    ))
}

/// A delta, paged by `maxChanges` so each page's `/get` of its created and updated ids stays
/// within `maxObjectsInGet`, until the server reports no more changes.
async fn paged_delta<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    since: &SyncState,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    let limit = page_size(fetch.executor);
    let mut since = since.clone();
    let mut fetched = Fetched::default();
    loop {
        let mut req = Request::new(fetch.using.iter().copied());
        let changes_method = format!("{}/changes", fetch.type_name);
        let changes_id = req.invoke(
            changes_method.clone(),
            json!({ "accountId": fetch.account, "sinceState": since.as_str(), "maxChanges": limit }),
        );
        let [created, updated] = ["/created", "/updated"].map(|path| {
            req.invoke(
                format!("{}/get", fetch.type_name),
                back_ref_get(
                    fetch.account,
                    &changes_id,
                    &changes_method,
                    path,
                    fetch.properties,
                ),
            )
        });
        let resp = fetch.executor.execute(&req).await?;
        let diff = Changes::parse(resp.result(&changes_id)?)?;
        fetched.upsert(objects(resp.result(&created)?, normalize)?);
        fetched.upsert(objects(resp.result(&updated)?, normalize)?);
        fetched.destroy(diff.destroyed);
        if diff.has_more && diff.new_state == since {
            return Err(JmapError::protocol(format!(
                "{changes_method} reported more changes without moving its state"
            )));
        }
        since = diff.new_state;
        if !diff.has_more {
            break;
        }
    }
    let (changed, removed) = fetched.into_parts();
    Ok(ScopeSync::new(SyncUpdate::delta(changed, removed), since))
}

/// What the pages of one pass have said so far, the later page winning for each key.
struct Fetched<T> {
    changed: BTreeMap<ProviderKey, T>,
    removed: BTreeSet<ProviderKey>,
}

impl<T> Default for Fetched<T> {
    fn default() -> Self {
        Self {
            changed: BTreeMap::new(),
            removed: BTreeSet::new(),
        }
    }
}

impl<T: Keyed> Fetched<T> {
    fn upsert(&mut self, objects: Vec<T>) {
        for object in objects {
            let key = object.provider_key().clone();
            self.removed.remove(&key);
            self.changed.insert(key, object);
        }
    }

    fn destroy(&mut self, keys: Vec<ProviderKey>) {
        for key in keys {
            self.changed.remove(&key);
            self.removed.insert(key);
        }
    }

    fn into_changed(self) -> Vec<T> {
        self.changed.into_values().collect()
    }

    fn into_parts(self) -> (Vec<T>, Vec<ProviderKey>) {
        (
            self.changed.into_values().collect(),
            self.removed.into_iter().collect(),
        )
    }
}
