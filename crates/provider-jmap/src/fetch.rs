//! The paged JMAP fetch primitives: the [`MemberPass`] behind `stream_email`, the `Foo/changes`
//! page and its back-referenced `Foo/get`s, and a `Foo/get` by id.
//!
//! A snapshot's walk over `Foo/query` is [`crate::walk`]; whole-scope sync, which loops these
//! for a [`engine_provider::ScopeSync`], is [`crate::fetch_scope`].

use engine_core::{
    error::FailureClass,
    ids::ProviderKey,
    sync::{SyncObject, SyncState},
};
use engine_provider::SyncKind;
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, result_ref, with_back_reference},
    sync_ops::{Changes, clamp_limit, objects},
    walk::{Order, Walk},
};

/// The fixed configuration for one type's paged fetch — everything that is
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

/// One pass of a member type for [`engine_provider::Provider::stream_email`], a page at a time.
///
/// The first page decides the pass: a delta from `cursor`, or a snapshot when there is none or
/// the server can no longer calculate changes from it (`cannotCalculateChanges`). A snapshot is a
/// [`Walk`] in [`Order::Fixed`], since the caller sorts on a property no update changes, and
/// records the state read with its first page. The pass is held in memory, so nothing outside
/// it parses where it is.
pub(crate) struct MemberPass<'f, 'a> {
    fetch: &'f MemberFetch<'a>,
    sort: Value,
    filter: Option<Value>,
    limit: usize,
    step: Step<'f, 'a>,
}

enum Step<'f, 'a> {
    Start(Option<SyncState>),
    Snapshot(Walk<'f, 'a>),
    Delta(SyncState),
    Done,
}

/// One page of a [`MemberPass`]; `next_cursor` is set on the pass's last page.
pub(crate) struct PassPage<T: SyncObject> {
    /// Whether the pass is a snapshot or a delta.
    pub(crate) kind: SyncKind,
    /// Objects new to the store, or replacing what it holds.
    pub(crate) changed: Vec<T>,
    /// State changes to objects the store holds.
    pub(crate) patched: Vec<T::Patch>,
    /// Objects the server destroyed.
    pub(crate) removed: Vec<ProviderKey>,
    /// A snapshot page's share of the scope's present set.
    pub(crate) present: Vec<ProviderKey>,
    /// The scope's size, when the server counted it.
    pub(crate) total: Option<usize>,
    /// The cursor the pass ends on, on its last page only.
    pub(crate) next_cursor: Option<SyncState>,
}

impl<'f, 'a> MemberPass<'f, 'a> {
    /// A pass from `cursor`, `limit` objects a page (clamped to `maxObjectsInGet`). `sort` must
    /// be on properties no update changes; `filter` bounds a snapshot.
    pub(crate) fn new(
        fetch: &'f MemberFetch<'a>,
        sort: Value,
        filter: Option<Value>,
        limit: usize,
        cursor: Option<&SyncState>,
    ) -> Self {
        Self {
            fetch,
            sort,
            filter,
            limit: clamp_limit(limit, fetch.executor.session().limits().max_objects_in_get),
            step: Step::Start(cursor.cloned()),
        }
    }

    /// The next page. Call only until a page carries `next_cursor`.
    ///
    /// # Errors
    ///
    /// The requests' own errors; [`JmapError::Unsettled`] from a snapshot walk.
    pub(crate) async fn next<T: SyncObject>(
        &mut self,
        normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
        updated: &UpdatedAsState<T::Patch>,
    ) -> Result<PassPage<T>, JmapError> {
        match std::mem::replace(&mut self.step, Step::Done) {
            Step::Start(None) => self.snapshot(self.walk(), normalize).await,
            Step::Start(Some(since)) => {
                match delta_page(self.fetch, &since, self.limit, normalize, updated).await {
                    Err(e) if e.failure_class() == FailureClass::NeedsResync => {
                        self.snapshot(self.walk(), normalize).await
                    }
                    other => Ok(self.delta(other?)),
                }
            }
            Step::Snapshot(walk) => self.snapshot(walk, normalize).await,
            Step::Delta(since) => {
                match delta_page(self.fetch, &since, self.limit, normalize, updated).await {
                    // Stuck past the first page: end the pass where it is, so the next one starts
                    // from here, meets the same answer on its first page and recovers with a
                    // snapshot. Everything up to here has been handed on.
                    Err(e) if e.failure_class() == FailureClass::NeedsResync => Ok(PassPage {
                        kind: SyncKind::Delta,
                        changed: Vec::new(),
                        patched: Vec::new(),
                        removed: Vec::new(),
                        present: Vec::new(),
                        total: None,
                        next_cursor: Some(since),
                    }),
                    other => Ok(self.delta(other?)),
                }
            }
            Step::Done => Err(JmapError::protocol("a member pass was asked past its end")),
        }
    }

    fn walk(&self) -> Walk<'f, 'a> {
        Walk::new(
            self.fetch,
            Some(self.sort.clone()),
            self.filter.clone(),
            Order::Fixed,
            self.limit,
        )
    }

    async fn snapshot<T: SyncObject>(
        &mut self,
        mut walk: Walk<'f, 'a>,
        normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    ) -> Result<PassPage<T>, JmapError> {
        let mut page = walk.next(normalize).await?;
        let next_cursor = if walk.is_done() {
            let missed = walk.settle(normalize).await?;
            page.changed.extend(missed.changed);
            page.present.extend(missed.present);
            Some(walk.cursor()?)
        } else {
            self.step = Step::Snapshot(walk);
            None
        };
        Ok(PassPage {
            kind: SyncKind::Snapshot,
            changed: page.changed,
            patched: Vec::new(),
            removed: Vec::new(),
            present: page.present,
            total: page.total,
            next_cursor,
        })
    }

    fn delta<T: SyncObject>(&mut self, page: DeltaPage<T>) -> PassPage<T> {
        let next_cursor = if page.has_more {
            self.step = Step::Delta(page.new_state);
            None
        } else {
            Some(page.new_state)
        };
        PassPage {
            kind: SyncKind::Delta,
            changed: page.changed,
            patched: page.patched,
            removed: page.removed,
            present: Vec::new(),
            total: None,
            next_cursor,
        }
    }
}

/// One `Foo/changes` page, its created ids fetched whole and its updated ids as state.
struct DeltaPage<T: SyncObject> {
    changed: Vec<T>,
    patched: Vec<T::Patch>,
    removed: Vec<ProviderKey>,
    new_state: SyncState,
    has_more: bool,
}

/// Fetches `ids` of `fetch`'s type by name, with `fetch.properties`. An id destroyed since it was
/// listed is in the result's `notFound`, and simply absent here.
pub(crate) async fn get_ids<T>(
    fetch: &MemberFetch<'_>,
    ids: &[ProviderKey],
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
) -> Result<Vec<T>, JmapError> {
    let mut req = Request::new(fetch.using.iter().copied());
    let mut args = Map::new();
    args.insert("accountId".to_owned(), json!(fetch.account));
    args.insert(
        "ids".to_owned(),
        json!(ids.iter().map(ProviderKey::as_str).collect::<Vec<_>>()),
    );
    if let Some(properties) = fetch.properties {
        args.insert("properties".to_owned(), json!(properties));
    }
    let get = req.invoke(format!("{}/get", fetch.type_name), Value::Object(args));
    let resp = fetch.executor.execute(&req).await?;
    objects(resp.result(&get)?, normalize)
}

/// One delta page: `Foo/changes` (bounded by `maxChanges`) resolved into created/updated
/// objects via two `Foo/get` back-references. A page that claims more changes without moving its
/// state is answered as `cannotCalculateChanges` (RFC 8620 §5.2), the answer a server with no
/// intermediate state to offer gives, so it is never paged forever.
async fn delta_page<T: SyncObject>(
    fetch: &MemberFetch<'_>,
    since: &SyncState,
    limit: usize,
    normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    updated_handling: &UpdatedAsState<T::Patch>,
) -> Result<DeltaPage<T>, JmapError> {
    // An updated id costs only the type's mutable half.
    let call = changes_request(fetch, since, limit, Some(updated_handling.properties));
    let resp = fetch.executor.execute(&call.request).await?;
    let diff = Changes::parse(resp.result(&call.changes)?)?;
    if diff.has_more && diff.new_state == *since {
        return Err(cannot_calculate_changes(call.changes));
    }
    Ok(DeltaPage {
        changed: objects(resp.result(&call.created)?, normalize)?,
        patched: objects(resp.result(&call.updated)?, updated_handling.normalize)?,
        removed: diff.destroyed,
        new_state: diff.new_state,
        has_more: diff.has_more,
    })
}

/// The `cannotCalculateChanges` method error (RFC 8620 §5.2) for `call_id`, which the caller
/// recovers from with a snapshot.
pub(crate) fn cannot_calculate_changes(call_id: String) -> JmapError {
    JmapError::Method {
        call_id,
        error_type: "cannotCalculateChanges".to_owned(),
    }
}

/// One `Foo/changes` page, bounded by `maxChanges`, and the two `Foo/get`s that resolve its
/// created and updated ids by back-reference, in one request.
pub(crate) struct ChangesRequest {
    /// The request to execute.
    pub(crate) request: Request,
    /// The `Foo/changes` call id.
    pub(crate) changes: String,
    /// The call id of the `Foo/get` over the created ids.
    pub(crate) created: String,
    /// The call id of the `Foo/get` over the updated ids.
    pub(crate) updated: String,
}

/// Builds a [`ChangesRequest`] from `since`; `updated_properties` narrows the `Foo/get` of the
/// updated ids, while the created ids are read with `fetch.properties`.
pub(crate) fn changes_request(
    fetch: &MemberFetch<'_>,
    since: &SyncState,
    limit: usize,
    updated_properties: Option<&[&str]>,
) -> ChangesRequest {
    let mut request = Request::new(fetch.using.iter().copied());
    let changes_method = format!("{}/changes", fetch.type_name);
    let changes = request.invoke(
        changes_method.clone(),
        json!({ "accountId": fetch.account, "sinceState": since.as_str(), "maxChanges": limit }),
    );
    let mut get = |path: &str, properties: Option<&[&str]>| {
        request.invoke(
            format!("{}/get", fetch.type_name),
            back_ref_get(fetch.account, &changes, &changes_method, path, properties),
        )
    };
    let created = get("/created", fetch.properties);
    let updated = get("/updated", updated_properties);
    ChangesRequest {
        request,
        changes,
        created,
        updated,
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
