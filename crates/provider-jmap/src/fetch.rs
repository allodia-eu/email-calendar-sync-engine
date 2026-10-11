//! The paged JMAP fetch primitives: one `Foo/query` page with its `Foo/get`, the paged
//! `member_page` behind `stream_email`, and the `#ids` back-reference both use.
//!
//! Whole-scope sync, which loops these pages for a [`engine_provider::ScopeSync`], is
//! [`crate::fetch_scope`].

use engine_core::{
    error::FailureClass,
    ids::ProviderKey,
    sync::{SyncObject, SyncState},
};
use engine_provider::{PageToken, SyncKind, SyncPage};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, result_ref, with_back_reference},
    sync_ops::{Changes, clamp_limit, keys, next_position, objects, state, total},
};

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

/// How a delta resolves its **`updated`** ids.
///
/// `Foo/changes` splits its answer into created, updated and destroyed, and the third of those is
/// the one worth being careful about. An updated id is *not* a new object: for `Email` it cannot
/// even be a changed one, because RFC 8621 §4.1 makes `keywords` and `mailboxIds` the only
/// mutable properties. So it is read through a narrow property list and becomes a state change,
/// and the payload holding the message's content is never rewritten to move a flag.
///
/// Carried as a parameter rather than assumed, because it is a fact about the *type* being
/// paged, not about JMAP.
pub(crate) struct UpdatedAsState<P> {
    /// The narrow `Foo/get` property list — the type's whole mutable half.
    pub(crate) properties: &'static [&'static str],
    /// Builds the state change one returned object describes.
    pub(crate) normalize: fn(&Value) -> Result<P, JmapError>,
}

/// Fetches **one page** of a member type — the paged primitive behind
/// [`engine_provider::Provider::stream_email`].
///
/// `sort` is the `Foo/query` comparator array (e.g. `receivedAt` descending so a
/// fresh sync surfaces recent objects first). `page` carries the continuation
/// from the previous page (`None` starts the pass) and `cursor` selects
/// snapshot-vs-delta on that first page. A first delta page that hits
/// `cannotCalculateChanges` recovers to a snapshot; the chosen mode and offset
/// then travel inside the opaque [`PageToken`], so the engine never parses it and
/// a recovered pass stays a snapshot to its end.
#[allow(
    clippy::too_many_arguments,
    reason = "the page inputs are all distinct and none has a sensible default"
)]
pub(crate) async fn member_page<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    sort: Value,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    limit: usize,
    filter: Option<&Value>,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    updated: &UpdatedAsState<T::Patch>,
) -> Result<SyncPage<T>, JmapError> {
    let limit = clamp_limit(limit, fetch.executor.session().limits().max_objects_in_get);
    match page {
        // A continuation page: the token carries the mode and offset forward.
        Some(token) => match PageCursor::parse(token)? {
            PageCursor::Snapshot(position) => {
                snapshot_page(fetch, sort, position, limit, filter, normalize).await
            }
            PageCursor::Delta(since) => delta_page(fetch, &since, limit, normalize, updated).await,
        },
        // The first page: a snapshot when there is no cursor, otherwise a delta
        // that recovers to a snapshot on `cannotCalculateChanges`.
        None => match cursor {
            None => snapshot_page(fetch, sort, 0, limit, filter, normalize).await,
            Some(since) => match delta_page(fetch, since, limit, normalize, updated).await {
                Err(e) if e.failure_class() == FailureClass::NeedsResync => {
                    snapshot_page(fetch, sort, 0, limit, filter, normalize).await
                }
                other => other,
            },
        },
    }
}

/// One `Foo/query` page and the objects it named, before it becomes a [`SyncPage`] or part of
/// a whole-scope snapshot.
pub(crate) struct QueryPage<T> {
    /// The objects the page's `Foo/get` returned.
    pub(crate) changed: Vec<T>,
    /// The ids the query returned, in order: the page's share of the scope's `present` set.
    pub(crate) present: Vec<ProviderKey>,
    /// The scope's size, when the server calculated it.
    pub(crate) total: Option<usize>,
    /// Where the next page starts, or `None` once the scope is exhausted.
    pub(crate) next_position: Option<usize>,
    /// The type's state, read in the same request as the query.
    pub(crate) state: SyncState,
    /// The query's own state (RFC 8620 §5.5): a different one on a later page means the
    /// results moved between the two.
    pub(crate) query_state: Option<String>,
}

/// One page: `Foo/query` (optionally sorted and filtered, at `position`, bounded by `limit`)
/// plus a `Foo/get` over the page's `#ids`, in one request.
pub(crate) async fn query_page<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    sort: Option<&Value>,
    position: usize,
    limit: usize,
    filter: Option<&Value>,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
) -> Result<QueryPage<T>, JmapError> {
    let mut req = Request::new(fetch.using.iter().copied());
    let query_method = format!("{}/query", fetch.type_name);
    let mut query_args = Map::new();
    query_args.insert("accountId".to_owned(), json!(fetch.account));
    if let Some(sort) = sort {
        query_args.insert("sort".to_owned(), sort.clone());
    }
    // A sync-depth window bounds the snapshot to recent mail via a `receivedAt`
    // filter (RFC 8621 §4.4.1). `Email/changes` takes no filter, so a delta applies none
    // and the orchestrator drops an out-of-window arrival on apply instead.
    if let Some(filter) = filter {
        query_args.insert("filter".to_owned(), filter.clone());
    }
    query_args.insert("position".to_owned(), json!(position));
    query_args.insert("limit".to_owned(), json!(limit));
    query_args.insert("calculateTotal".to_owned(), json!(true));
    let query = req.invoke(query_method.clone(), Value::Object(query_args));
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
    Ok(QueryPage {
        changed: objects(get_result, normalize)?,
        next_position: next_position(position, limit, present.len(), total),
        present,
        total,
        state: state(get_result, "state")?,
        query_state: query_result
            .get("queryState")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// One snapshot page of a paged pass; [`next_position`] decides whether another follows.
async fn snapshot_page<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    sort: Value,
    position: usize,
    limit: usize,
    filter: Option<&Value>,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
) -> Result<SyncPage<T>, JmapError> {
    let page = query_page(fetch, Some(&sort), position, limit, filter, normalize).await?;
    Ok(SyncPage {
        kind: SyncKind::Snapshot,
        changed: page.changed,
        patched: Vec::new(),
        removed: Vec::new(),
        present: page.present,
        next_page: page
            .next_position
            .map(|p| PageCursor::Snapshot(p).to_token()),
        next_cursor: page.state,
        total: page.total,
    })
}

/// One delta page: `Foo/changes` (bounded by `maxChanges`) resolved into created/
/// updated objects via two `Foo/get` back-references. `hasMoreChanges` decides
/// whether another page follows, resuming from the page's `newState`.
async fn delta_page<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    since: &SyncState,
    limit: usize,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    updated_handling: &UpdatedAsState<T::Patch>,
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
    // An updated id costs only the type's mutable half.
    let updated = req.invoke(
        format!("{}/get", fetch.type_name),
        back_ref_get(
            fetch.account,
            &changes_id,
            &changes_method,
            "/updated",
            Some(updated_handling.properties),
        ),
    );
    let resp = fetch.executor.execute(&req).await?;
    let diff = Changes::parse(resp.result(&changes_id)?)?;
    let changed = objects(resp.result(&created)?, normalize)?;
    let patched = objects(resp.result(&updated)?, updated_handling.normalize)?;
    let next_page = diff
        .has_more
        .then(|| PageCursor::Delta(diff.new_state.clone()).to_token());
    Ok(SyncPage {
        kind: SyncKind::Delta,
        changed,
        patched,
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
pub(crate) fn back_ref_get(
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
#[path = "fetch_tests.rs"]
mod tests;
