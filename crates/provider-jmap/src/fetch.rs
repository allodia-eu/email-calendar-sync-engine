//! Generic JMAP read/sync orchestration shared by mail and calendar.
//!
//! Two shapes cover every type: **containers** (`Mailbox`, `Calendar`) sync via
//! `Foo/get` (snapshot) or `Foo/changes` → `Foo/get` (delta); **members**
//! (`Email`, `CalendarEvent`) via `Foo/query` → `Foo/get` (snapshot) or
//! `Foo/changes` → `Foo/get` (delta). Both recover from `cannotCalculateChanges`
//! by falling back to a snapshot, and use `#ids` result back-references so a
//! change set is fetched in one round trip. The per-type difference is just the
//! method-name prefix, the capability set, and the normalizer.

use std::collections::BTreeSet;

use engine_core::error::FailureClass;
use engine_core::ids::ProviderKey;
use engine_core::sync::{SyncState, SyncUpdate};
use engine_provider::{PageToken, ScopeSync, SyncKind, SyncPage};
use serde_json::{Map, Value, json};

use crate::error::JmapError;
use crate::provider::Executor;
use crate::request::{Request, result_ref, with_back_reference};
use crate::sync_ops::{
    Changes, clamp_limit, is_complete, key_set, keys, next_position, objects, snapshot_or_delta,
    state, total,
};

/// Syncs a container type (`Foo/get` snapshot; `Foo/changes`+`Foo/get` delta),
/// recovering to a snapshot on `cannotCalculateChanges`.
pub(crate) async fn container_sync<T>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    cursor: Option<&SyncState>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    key_of: impl Fn(&T) -> ProviderKey + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    let Some(cursor) = cursor else {
        return container_snapshot(executor, account, using, type_name, normalize, key_of).await;
    };
    match container_delta(executor, account, using, type_name, cursor, normalize).await {
        Err(e) if e.failure_class() == FailureClass::NeedsResync => {
            container_snapshot(executor, account, using, type_name, normalize, key_of).await
        }
        other => other,
    }
}

async fn container_snapshot<T>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
    key_of: impl Fn(&T) -> ProviderKey,
) -> Result<ScopeSync<T>, JmapError> {
    let mut req = Request::new(using.iter().copied());
    let get = req.invoke(format!("{type_name}/get"), json!({ "accountId": account }));
    let resp = executor.execute(&req).await?;
    let result = resp.result(&get)?;
    let items = objects(result, normalize)?;
    let present: BTreeSet<ProviderKey> = items.iter().map(key_of).collect();
    Ok(ScopeSync::new(
        SyncUpdate::snapshot(items, present),
        state(result, "state")?,
    ))
}

async fn container_delta<T>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    cursor: &SyncState,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    let mut req = Request::new(using.iter().copied());
    let changes_method = format!("{type_name}/changes");
    let changes_id = req.invoke(
        changes_method.clone(),
        json!({ "accountId": account, "sinceState": cursor.as_str() }),
    );
    let created = req.invoke(
        format!("{type_name}/get"),
        back_ref_get(account, &changes_id, &changes_method, "/created", None),
    );
    let updated = req.invoke(
        format!("{type_name}/get"),
        back_ref_get(account, &changes_id, &changes_method, "/updated", None),
    );
    let resp = executor.execute(&req).await?;
    let diff = Changes::parse(resp.result(&changes_id)?)?;
    let mut changed = objects(resp.result(&created)?, normalize)?;
    changed.extend(objects(resp.result(&updated)?, normalize)?);
    Ok(ScopeSync::new(
        SyncUpdate::delta(changed, diff.destroyed),
        diff.new_state,
    ))
}

/// Syncs a member type (`Foo/query`+`Foo/get` snapshot; `Foo/changes`+`Foo/get`
/// delta), recovering to a snapshot on `cannotCalculateChanges`.
pub(crate) async fn member_sync<T>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    properties: Option<&'static [&'static str]>,
    cursor: Option<&SyncState>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    let Some(cursor) = cursor else {
        return member_snapshot(executor, account, using, type_name, properties, normalize).await;
    };
    match member_delta(
        executor, account, using, type_name, properties, cursor, normalize,
    )
    .await
    {
        Err(e) if e.failure_class() == FailureClass::NeedsResync => {
            member_snapshot(executor, account, using, type_name, properties, normalize).await
        }
        other => other,
    }
}

async fn member_snapshot<T>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    properties: Option<&'static [&'static str]>,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
) -> Result<ScopeSync<T>, JmapError> {
    let mut req = Request::new(using.iter().copied());
    let query_method = format!("{type_name}/query");
    let query = req.invoke(
        query_method.clone(),
        json!({ "accountId": account, "calculateTotal": true }),
    );
    let get = req.invoke(
        format!("{type_name}/get"),
        back_ref_get(account, &query, &query_method, "/ids", properties),
    );
    let resp = executor.execute(&req).await?;
    let query_result = resp.result(&query)?;
    let present = key_set(query_result, "ids")?;
    let total = total(query_result);
    let get_result = resp.result(&get)?;
    let cursor = state(get_result, "state")?;
    let items = objects(get_result, normalize)?;
    let complete = is_complete(total, present.len());
    Ok(ScopeSync::new(
        snapshot_or_delta(items, present, complete),
        cursor,
    ))
}

async fn member_delta<T>(
    executor: &dyn Executor,
    account: &str,
    using: &[&'static str],
    type_name: &str,
    properties: Option<&'static [&'static str]>,
    cursor: &SyncState,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<ScopeSync<T>, JmapError> {
    let mut req = Request::new(using.iter().copied());
    let changes_method = format!("{type_name}/changes");
    let changes_id = req.invoke(
        changes_method.clone(),
        json!({ "accountId": account, "sinceState": cursor.as_str() }),
    );
    let created = req.invoke(
        format!("{type_name}/get"),
        back_ref_get(
            account,
            &changes_id,
            &changes_method,
            "/created",
            properties,
        ),
    );
    let updated = req.invoke(
        format!("{type_name}/get"),
        back_ref_get(
            account,
            &changes_id,
            &changes_method,
            "/updated",
            properties,
        ),
    );
    let resp = executor.execute(&req).await?;
    let diff = Changes::parse(resp.result(&changes_id)?)?;
    let mut changed = objects(resp.result(&created)?, normalize)?;
    changed.extend(objects(resp.result(&updated)?, normalize)?);
    Ok(ScopeSync::new(
        SyncUpdate::delta(changed, diff.destroyed),
        diff.new_state,
    ))
}

/// The fixed configuration for one member type's paged fetch — everything that is
/// constant across the pages of a pass. Bundled so each page builder takes a sane
/// argument count and the per-call inputs (sort, cursor, page, limit) stand out.
pub(crate) struct MemberFetch<'a> {
    /// The method executor (live client or, in tests, a fake).
    pub(crate) executor: &'a dyn Executor,
    /// The JMAP (server-side) account id for method arguments.
    pub(crate) account: &'a str,
    /// The capability URNs the methods rely on.
    pub(crate) using: &'a [&'static str],
    /// The JMAP type name prefix, e.g. `Email`.
    pub(crate) type_name: &'a str,
    /// The `Foo/get` properties to fetch, or `None` for all.
    pub(crate) properties: Option<&'static [&'static str]>,
}

/// Fetches **one page** of a member type — the paged primitive behind
/// [`engine_provider::Provider::sync_email_page`].
///
/// `sort` is the `Foo/query` comparator array (e.g. `receivedAt` descending so a
/// fresh sync surfaces recent objects first). `page` carries the continuation
/// from the previous page (`None` starts the pass) and `cursor` selects
/// snapshot-vs-delta on that first page. A first delta page that hits
/// `cannotCalculateChanges` recovers to a snapshot; the chosen mode and offset
/// then travel inside the opaque [`PageToken`], so the engine never parses it and
/// a recovered pass stays a snapshot to its end.
pub(crate) async fn member_page<T>(
    fetch: &MemberFetch<'_>,
    sort: Value,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    limit: usize,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<SyncPage<T>, JmapError> {
    let limit = clamp_limit(limit, fetch.executor.session().limits().max_objects_in_get);
    match page {
        // A continuation page: the token carries the mode and offset forward.
        Some(token) => match PageCursor::parse(token)? {
            PageCursor::Snapshot(position) => {
                snapshot_page(fetch, sort, position, limit, normalize).await
            }
            PageCursor::Delta(since) => delta_page(fetch, &since, limit, normalize).await,
        },
        // The first page: a snapshot when there is no cursor, otherwise a delta
        // that recovers to a snapshot on `cannotCalculateChanges`.
        None => match cursor {
            None => snapshot_page(fetch, sort, 0, limit, normalize).await,
            Some(since) => match delta_page(fetch, since, limit, normalize).await {
                Err(e) if e.failure_class() == FailureClass::NeedsResync => {
                    snapshot_page(fetch, sort, 0, limit, normalize).await
                }
                other => other,
            },
        },
    }
}

/// One snapshot page: `Foo/query` (sorted, at `position`, bounded by `limit`) plus
/// a `Foo/get` over the resulting ids. The query ids are the page's `present` set;
/// [`next_position`] decides whether another page follows.
async fn snapshot_page<T>(
    fetch: &MemberFetch<'_>,
    sort: Value,
    position: usize,
    limit: usize,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
) -> Result<SyncPage<T>, JmapError> {
    let mut req = Request::new(fetch.using.iter().copied());
    let query_method = format!("{}/query", fetch.type_name);
    let query = req.invoke(
        query_method.clone(),
        json!({
            "accountId": fetch.account,
            "sort": sort,
            "position": position,
            "limit": limit,
            "calculateTotal": true,
        }),
    );
    let get = req.invoke(
        format!("{}/get", fetch.type_name),
        back_ref_get(
            fetch.account,
            &query,
            &query_method,
            "/ids",
            fetch.properties,
        ),
    );
    let resp = fetch.executor.execute(&req).await?;
    let query_result = resp.result(&query)?;
    let present = keys(query_result, "ids")?;
    let total = total(query_result);
    let get_result = resp.result(&get)?;
    let next_cursor = state(get_result, "state")?;
    let changed = objects(get_result, normalize)?;
    let next_page = next_position(position, limit, present.len(), total)
        .map(|p| PageCursor::Snapshot(p).to_token());
    Ok(SyncPage {
        kind: SyncKind::Snapshot,
        changed,
        removed: Vec::new(),
        present,
        next_page,
        next_cursor,
        total,
    })
}

/// One delta page: `Foo/changes` (bounded by `maxChanges`) resolved into created/
/// updated objects via two `Foo/get` back-references. `hasMoreChanges` decides
/// whether another page follows, resuming from the page's `newState`.
async fn delta_page<T>(
    fetch: &MemberFetch<'_>,
    since: &SyncState,
    limit: usize,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
) -> Result<SyncPage<T>, JmapError> {
    let mut req = Request::new(fetch.using.iter().copied());
    let changes_method = format!("{}/changes", fetch.type_name);
    let changes_id = req.invoke(
        changes_method.clone(),
        json!({ "accountId": fetch.account, "sinceState": since.as_str(), "maxChanges": limit }),
    );
    let created = req.invoke(
        format!("{}/get", fetch.type_name),
        back_ref_get(
            fetch.account,
            &changes_id,
            &changes_method,
            "/created",
            fetch.properties,
        ),
    );
    let updated = req.invoke(
        format!("{}/get", fetch.type_name),
        back_ref_get(
            fetch.account,
            &changes_id,
            &changes_method,
            "/updated",
            fetch.properties,
        ),
    );
    let resp = fetch.executor.execute(&req).await?;
    let diff = Changes::parse(resp.result(&changes_id)?)?;
    let mut changed = objects(resp.result(&created)?, normalize)?;
    changed.extend(objects(resp.result(&updated)?, normalize)?);
    let next_page = diff
        .has_more
        .then(|| PageCursor::Delta(diff.new_state.clone()).to_token());
    Ok(SyncPage {
        kind: SyncKind::Delta,
        changed,
        removed: diff.destroyed,
        present: Vec::new(),
        next_page,
        next_cursor: diff.new_state,
        total: None,
    })
}

/// The mode and offset a [`PageToken`] carries between member pages, so a paused
/// pass resumes exactly where it left off. The adapter encodes it on one page and
/// decodes it on the next; the engine treats the token as opaque.
enum PageCursor {
    /// Snapshot paging resuming `Foo/query` at this position.
    Snapshot(usize),
    /// Delta paging resuming `Foo/changes` from this state.
    Delta(SyncState),
}

impl PageCursor {
    /// Encodes the continuation as an opaque [`PageToken`].
    fn to_token(&self) -> PageToken {
        match self {
            Self::Snapshot(position) => PageToken::new(format!("s:{position}")),
            Self::Delta(state) => PageToken::new(format!("d:{}", state.as_str())),
        }
    }

    /// Decodes a [`PageToken`] this adapter previously produced.
    fn parse(token: &PageToken) -> Result<Self, JmapError> {
        let raw = token.as_str();
        if let Some(position) = raw.strip_prefix("s:") {
            position
                .parse::<usize>()
                .map(Self::Snapshot)
                .map_err(|_| JmapError::protocol(format!("bad snapshot page token {raw:?}")))
        } else if let Some(state) = raw.strip_prefix("d:") {
            Ok(Self::Delta(SyncState::new(state)))
        } else {
            Err(JmapError::protocol(format!("unknown page token {raw:?}")))
        }
    }
}

/// Builds a `Foo/get` argument object that fetches the ids a prior call produced,
/// via a `#ids` result back-reference (RFC 8620 §3.7).
fn back_ref_get(
    account: &str,
    result_of: &str,
    name: &str,
    path: &str,
    properties: Option<&[&str]>,
) -> Value {
    let mut args = Map::new();
    args.insert("accountId".to_owned(), json!(account));
    if let Some(props) = properties {
        args.insert("properties".to_owned(), json!(props));
    }
    with_back_reference(args, "ids", result_ref(result_of, name, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_cursor_round_trips_through_its_opaque_token() {
        // Snapshot offsets and delta states survive encode → decode unchanged.
        let snap = PageCursor::Snapshot(42).to_token();
        assert_eq!(snap.as_str(), "s:42");
        assert!(matches!(
            PageCursor::parse(&snap).unwrap(),
            PageCursor::Snapshot(42)
        ));

        let delta = PageCursor::Delta(SyncState::new("changes-state")).to_token();
        assert_eq!(delta.as_str(), "d:changes-state");
        match PageCursor::parse(&delta).unwrap() {
            PageCursor::Delta(state) => assert_eq!(state.as_str(), "changes-state"),
            PageCursor::Snapshot(_) => panic!("expected a delta cursor"),
        }
    }

    #[test]
    fn malformed_page_tokens_are_protocol_errors_not_panics() {
        // A non-numeric snapshot offset and an unknown prefix both error cleanly.
        assert!(PageCursor::parse(&PageToken::new("s:not-a-number")).is_err());
        assert!(PageCursor::parse(&PageToken::new("garbage")).is_err());
    }
}
