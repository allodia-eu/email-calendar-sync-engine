//! Whole-scope JMAP sync: every type a provider hands over as one [`ScopeSync`], however large.
//!
//! Two shapes cover every type. **Queried** types (`Mailbox`, `CalendarEvent`, `ContactCard`)
//! snapshot by walking `Foo/query` ([`crate::walk`]) in the server's own order, each page's ids
//! fetched by a `#ids` back-reference in the same request. **Containers** (`Calendar`,
//! `AddressBook`) snapshot with one `Foo/get`: neither has a `/query` to page (JMAP Calendars,
//! RFC 9610), so a server holding more of them than one `/get` may carry refuses the snapshot.
//! Both delta by paging `Foo/changes`. Every page asks for at most `maxObjectsInGet` ids (RFC 8620
//! §5.1), because a server refuses a larger `/get` outright with `requestTooLarge` rather than
//! truncating it, so a scope larger than one `/get` takes several requests. The walk's rules keep
//! a snapshot correct across them; a delta's pages are applied in order, so the last page to
//! mention an object decides whether it exists.

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
    fetch::{MemberFetch, cannot_calculate_changes, changes_request},
    request::Request,
    sync_ops::{Changes, objects, state},
    walk::{Order, Walk},
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
) -> Result<ScopeSync<T>, JmapError> {
    container_sync_with_status(executor, account, using, type_name, cursor, normalize)
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
) -> Result<(ScopeSync<T>, bool), JmapError> {
    let fetch = MemberFetch {
        executor,
        account,
        using,
        type_name,
        properties: None,
    };
    let Some(cursor) = cursor else {
        return container_snapshot(&fetch, normalize)
            .await
            .map(|sync| (sync, false));
    };
    match paged_delta(&fetch, cursor, normalize).await {
        Err(e) if e.failure_class() == FailureClass::NeedsResync => {
            container_snapshot(&fetch, normalize)
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
) -> Result<ScopeSync<T>, JmapError> {
    let mut req = Request::new(fetch.using.iter().copied());
    let get = req.invoke(
        format!("{}/get", fetch.type_name),
        json!({ "accountId": fetch.account }),
    );
    let resp = fetch.executor.execute(&req).await?;
    let result = resp.result(&get)?;
    let items = objects(result, normalize)?;
    let present: BTreeSet<ProviderKey> = items
        .iter()
        .map(|item| item.provider_key().clone())
        .collect();
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
    let mut walk = Walk::new(fetch, None, None, Order::Server, page_size(fetch.executor));
    let mut present = BTreeSet::new();
    let mut fetched = Fetched::default();
    while !walk.is_done() {
        let page = walk.next(normalize).await?;
        present.extend(page.present);
        fetched.upsert(page.changed);
    }
    let missed = walk.settle(normalize).await?;
    present.extend(missed.present);
    fetched.upsert(missed.changed);
    Ok(ScopeSync::new(
        SyncUpdate::snapshot(fetched.into_changed(), present),
        walk.cursor()?,
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
        let call = changes_request(fetch, &since, limit, fetch.properties);
        let resp = fetch.executor.execute(&call.request).await?;
        let diff = Changes::parse(resp.result(&call.changes)?)?;
        fetched.upsert(objects(resp.result(&call.created)?, normalize)?);
        fetched.upsert(objects(resp.result(&call.updated)?, normalize)?);
        fetched.destroy(diff.destroyed);
        if diff.has_more && diff.new_state == since {
            // A server with no intermediate state to offer answers `cannotCalculateChanges`
            // (RFC 8620 §5.2); claiming more at the same state is the same answer, so the caller
            // recovers with a snapshot rather than paging forever or failing every pass.
            return Err(cannot_calculate_changes(call.changes));
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
