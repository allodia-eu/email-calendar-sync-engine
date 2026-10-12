//! Walking a whole `Foo/query` for a snapshot, while other clients go on changing it.
//!
//! A snapshot larger than one `/get` takes several requests (RFC 8620 §5.1), and the account is
//! live between them. The walk stays correct across them by three rules:
//!
//! - **Each page starts after the last id the previous page returned**, as an `anchor` with
//!   `anchorOffset` 1 (§5.5), never at a `position`. A position counts from the top of the list, so
//!   a destroy above it moves every later id up one place and the walk skips one; an anchor moves
//!   with the list. An anchor that was itself destroyed is answered with `anchorNotFound`, and the
//!   walk steps back through the ids it was given, further each time ([`anchors`]); an id it is
//!   given again is not handed on twice.
//! - **The cursor is the type's `state` read with the first page**, so whatever changes during the
//!   walk is replayed by the next delta: a destroy, a create above the walk, a flag.
//! - **A walk of more than one page ends with a listing of the ids** ([`Walk::settle`]), and any
//!   the walk did not see are fetched. An object can move from ahead of the walk to behind it
//!   unseen, and a delta does not name it if it moved without changing. In the server's own order
//!   ([`Order::Server`]) an update can move it. In a sort no update changes ([`Order::Fixed`]) a
//!   server can still move it: Stalwart lists a message it has just stored last, and sorts it into
//!   place a moment later, leaving `queryState` as it was, so the move cannot be seen and the
//!   listing is taken every time. A single move is caught either way: the walk saw the object
//!   before it moved, or the listing sees it after. Where an update can move objects again, the
//!   listing must hold still (`queryState` unchanged across it), and one that never does fails the
//!   pass as one to retry ([`JmapError::Unsettled`]) rather than storing part of the scope.
//!
//! `Foo/queryChanges` (§5.6) would name what moved in one call, but a server need not offer it
//! (`canCalculateChanges`), and a server that moves results without moving `queryState` would
//! not name them there either; listing again needs nothing beyond `/query`.

use std::collections::BTreeSet;

use engine_core::{
    ids::ProviderKey,
    sync::{Keyed, SyncState},
};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    fetch::{MemberFetch, get_ids},
    request::{Request, result_ref, with_back_reference},
    sync_ops::{keys, objects, state, total},
};

/// How many pages in a row may bring nothing new, beyond the pages a step back re-reads, before
/// the walk gives up as unsettled.
const IDLE_PAGES: usize = 3;
/// How many times [`Walk::settle`] lists the ids again before the pass fails as unsettled.
const LISTINGS: usize = 3;

/// Whether an update can change an object's place in the query's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// Sorted on properties no update changes (`Email` by `receivedAt`, RFC 8621 §4.1).
    Fixed,
    /// The server's own order, which an update may change.
    Server,
}

/// What one page of a walk adds: only ids the walk had not seen, and their objects.
pub(crate) struct WalkPage<T> {
    /// The objects the page fetched.
    pub(crate) changed: Vec<T>,
    /// The ids the page added to the scope, in query order.
    pub(crate) present: Vec<ProviderKey>,
    /// The scope's size as the server last counted it.
    pub(crate) total: Option<usize>,
}

/// One walk over a whole `Foo/query`.
pub(crate) struct Walk<'f, 'a> {
    fetch: &'f MemberFetch<'a>,
    sort: Option<Value>,
    filter: Option<Value>,
    order: Order,
    limit: usize,
    /// `false` for the ids-only listing [`Walk::settle`] runs.
    with_objects: bool,
    cursor: Option<SyncState>,
    query_state: Option<String>,
    steady: bool,
    /// Every id the pages returned, in the order they returned them: where the next page is
    /// anchored, and where a destroyed anchor steps back to.
    trail: Vec<ProviderKey>,
    seen: BTreeSet<ProviderKey>,
    total: Option<usize>,
    pages: usize,
    done: bool,
    idle: usize,
    /// Pages a step back is expected to re-read before the walk is past where it was.
    reread: usize,
}

impl<'f, 'a> Walk<'f, 'a> {
    /// A walk over `fetch`'s type, `limit` ids a page.
    pub(crate) fn new(
        fetch: &'f MemberFetch<'a>,
        sort: Option<Value>,
        filter: Option<Value>,
        order: Order,
        limit: usize,
    ) -> Self {
        Self {
            fetch,
            sort,
            filter,
            order,
            limit: limit.max(1),
            with_objects: true,
            cursor: None,
            query_state: None,
            steady: true,
            trail: Vec::new(),
            seen: BTreeSet::new(),
            total: None,
            pages: 0,
            done: false,
            idle: 0,
            reread: 0,
        }
    }

    /// Whether the walk has reached the end of the query.
    pub(crate) const fn is_done(&self) -> bool {
        self.done
    }

    /// The type's state read with the first page: the cursor the snapshot records.
    ///
    /// # Errors
    ///
    /// A protocol error when no page has been fetched yet.
    pub(crate) fn cursor(&self) -> Result<SyncState, JmapError> {
        self.cursor
            .clone()
            .ok_or_else(|| JmapError::protocol("a snapshot walk recorded no state"))
    }

    /// Fetches the next page. Call only while [`Walk::is_done`] is `false`.
    ///
    /// # Errors
    ///
    /// The request's own errors, and [`JmapError::Unsettled`] when the walk cannot find its
    /// place again or stops making progress.
    pub(crate) async fn next<T: Keyed>(
        &mut self,
        normalize: impl Fn(&Value) -> Result<T, JmapError>,
    ) -> Result<WalkPage<T>, JmapError> {
        let (query, objects_result) = self.fetch_page().await?;
        let ids = keys(&query, "ids")?;
        let count = |field: &str| {
            query
                .get(field)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        let (position, page_limit) = (count("position"), count("limit").unwrap_or(self.limit));
        let query_state = query
            .get("queryState")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if self.pages == 0 {
            self.query_state = query_state;
        } else {
            self.steady &= query_state.is_some() && query_state == self.query_state;
        }
        self.pages += 1;
        self.total = total(&query).or(self.total);
        self.done = ids.is_empty()
            || match (self.total, position) {
                (Some(total), Some(position)) => position + ids.len() >= total,
                _ => ids.len() < page_limit,
            };
        let fresh: Vec<ProviderKey> = ids
            .iter()
            .filter(|id| !self.seen.contains(*id))
            .cloned()
            .collect();
        self.trail.extend(ids);
        let changed = match objects_result {
            Some(result) => {
                if self.cursor.is_none() {
                    self.cursor = Some(state(&result, "state")?);
                }
                let wanted: BTreeSet<&ProviderKey> = fresh.iter().collect();
                objects(&result, normalize)?
                    .into_iter()
                    .filter(|object| wanted.contains(object.provider_key()))
                    .collect()
            }
            None => Vec::new(),
        };
        if fresh.is_empty() && !self.done {
            self.idle += 1;
            if self.idle > self.reread + IDLE_PAGES {
                return Err(self.unsettled());
            }
        } else {
            self.idle = 0;
            self.reread = 0;
        }
        self.seen.extend(fresh.iter().cloned());
        Ok(WalkPage {
            changed,
            present: fresh,
            total: self.total,
        })
    }

    /// What the walk may have missed, once it is done: nothing for a walk of one page, whose
    /// query was answered at once; otherwise the objects a closing listing names that the walk
    /// did not see.
    ///
    /// # Errors
    ///
    /// The requests' own errors, and [`JmapError::Unsettled`] when no listing of a
    /// server-ordered query holds still.
    pub(crate) async fn settle<T: Keyed>(
        &mut self,
        normalize: impl Fn(&Value) -> Result<T, JmapError> + Copy,
    ) -> Result<WalkPage<T>, JmapError> {
        if self.pages <= 1 {
            return Ok(WalkPage {
                changed: Vec::new(),
                present: Vec::new(),
                total: self.total,
            });
        }
        for _ in 0..LISTINGS {
            let mut listing = Walk {
                with_objects: false,
                ..Walk::new(
                    self.fetch,
                    self.sort.clone(),
                    self.filter.clone(),
                    self.order,
                    self.limit,
                )
            };
            while !listing.is_done() {
                listing.next(normalize).await?;
            }
            let held_still = listing.steady && listing.query_state.is_some();
            if self.order == Order::Server && !held_still {
                continue;
            }
            let missed: Vec<ProviderKey> = listing
                .seen
                .into_iter()
                .filter(|id| !self.seen.contains(id))
                .collect();
            let mut changed = Vec::new();
            for chunk in missed.chunks(self.limit) {
                changed.extend(get_ids(self.fetch, chunk, normalize).await?);
            }
            self.seen.extend(missed.iter().cloned());
            return Ok(WalkPage {
                changed,
                present: missed,
                total: listing.total.or(self.total),
            });
        }
        Err(self.unsettled())
    }

    /// One page's `Foo/query` result and, when the walk fetches objects, its `Foo/get` result,
    /// stepping back along the trail while the anchor tried was destroyed.
    async fn fetch_page(&mut self) -> Result<(Value, Option<Value>), JmapError> {
        if self.trail.is_empty() {
            return self.request(None).await;
        }
        for back in anchors(self.trail.len()) {
            let anchor = &self.trail[self.trail.len() - 1 - back];
            match self.request(Some(anchor)).await {
                Err(JmapError::Method { error_type, .. }) if error_type == "anchorNotFound" => {}
                other => {
                    self.reread += back / self.limit;
                    return other;
                }
            }
        }
        // Every id the walk was given has been destroyed since: there is nowhere to resume.
        Err(self.unsettled())
    }

    async fn request(
        &self,
        anchor: Option<&ProviderKey>,
    ) -> Result<(Value, Option<Value>), JmapError> {
        let fetch = self.fetch;
        let mut req = Request::new(fetch.using.iter().copied());
        let query_method = format!("{}/query", fetch.type_name);
        let mut args = Map::new();
        args.insert("accountId".to_owned(), json!(fetch.account));
        if let Some(sort) = &self.sort {
            args.insert("sort".to_owned(), sort.clone());
        }
        if let Some(filter) = &self.filter {
            args.insert("filter".to_owned(), filter.clone());
        }
        match anchor {
            Some(anchor) => {
                args.insert("anchor".to_owned(), json!(anchor.as_str()));
                args.insert("anchorOffset".to_owned(), json!(1));
            }
            None => {
                args.insert("position".to_owned(), json!(0));
            }
        }
        args.insert("limit".to_owned(), json!(self.limit));
        args.insert("calculateTotal".to_owned(), json!(true));
        let query = req.invoke(query_method.clone(), Value::Object(args));
        let get = self.with_objects.then(|| {
            let mut get_args = Map::new();
            get_args.insert("accountId".to_owned(), json!(fetch.account));
            if let Some(properties) = fetch.properties {
                get_args.insert("properties".to_owned(), json!(properties));
            }
            req.invoke(
                format!("{}/get", fetch.type_name),
                with_back_reference(get_args, "ids", result_ref(&query, &query_method, "/ids")),
            )
        });
        let resp = fetch.executor.execute(&req).await?;
        let query_result = resp.result(&query)?.clone();
        let get_result = match get {
            Some(get) => Some(resp.result(&get)?.clone()),
            None => None,
        };
        Ok((query_result, get_result))
    }

    fn unsettled(&self) -> JmapError {
        JmapError::Unsettled(self.fetch.type_name.to_owned())
    }
}

/// How far back from the end of a trail of `len` ids each anchor tried sits: the last four,
/// then twice as far each time, down to the first. A destroy rarely takes more than the last
/// id, and a step back of `n` costs re-reading `n` ids, so the search stays short either way.
pub(crate) fn anchors(len: usize) -> impl Iterator<Item = usize> {
    let near = 0..len.min(4);
    let far = std::iter::successors(Some(8_usize), |back| back.checked_mul(2))
        .take_while(move |&back| back < len);
    let first = (len > 4).then(|| len - 1);
    near.chain(far)
        .chain(first)
        .scan(None, |last, back| {
            let repeat = *last == Some(back);
            *last = Some(back);
            Some((!repeat).then_some(back))
        })
        .flatten()
}
