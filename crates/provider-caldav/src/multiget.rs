//! Fetching the bodies a `sync-collection` answer left out.
//!
//! RFC 6578 §3.2 lets a server answer a sync with fewer properties than were asked for, and
//! iCloud does so for every member: each changed resource arrives with its `getetag` and without
//! its `calendar-data` or `address-data`. Such a member is fetched again with a multiget
//! (RFC 4791 §7.9, RFC 6352 §8.7), and its answer takes the bodiless one's place, so what follows
//! reads every changed member the same way whichever server sent it.

use std::collections::HashMap;

use futures_util::{StreamExt, stream};

use crate::{
    dav::MultiStatus,
    error::CalDavError,
    transport::{DavExecutor, DavMethod},
};

/// How many members one multiget names: enough that a collection of thousands costs tens of
/// requests on its first sync, few enough that no answer is unreasonably large.
const BATCH: usize = 100;

/// How many multigets are in flight at once. They are independent, and each answer is put back
/// by href, so the order they land in does not matter. Sent one after another, a first sync of
/// six thousand iCloud events waited out sixty round trips of most of a second each.
const IN_FLIGHT: usize = 4;

/// Fetches, with `multiget`, the body of every changed member of `report` that arrived without
/// its `data` property, and puts each answer in place of the member it fetched.
///
/// The collection's own entry is not a member and is left as it is. A member the multiget answers
/// with a `404` was removed after the sync listed it, and becomes a removal. Any other member the
/// multiget does not return with a body is left as it came, which is what the caller already
/// makes of a body it cannot read.
///
/// # Errors
///
/// Returns [`CalDavError`] when a multiget fails.
pub(crate) async fn fill_bodies(
    exec: &dyn DavExecutor,
    collection: &str,
    report: &mut MultiStatus,
    data: &str,
    multiget: fn(&[String]) -> String,
) -> Result<(), CalDavError> {
    let missing: Vec<String> = report
        .responses
        .iter()
        .filter(|response| {
            !response.is_removed()
                && response.props.get(data).is_none()
                && !same_path(response.href(), collection)
        })
        .map(|response| response.href().to_owned())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let slots: HashMap<String, usize> = report
        .responses
        .iter()
        .enumerate()
        .map(|(at, response)| (path_key(response.href()), at))
        .collect();
    let requests: Vec<_> = missing
        .chunks(BATCH)
        .map(|batch| fetch(exec, collection, multiget(batch)))
        .collect();
    let mut answers = stream::iter(requests).buffer_unordered(IN_FLIGHT);
    while let Some(fetched) = answers.next().await {
        for found in fetched?.responses {
            if found.is_removed() {
                for href in &found.hrefs {
                    if let Some(&at) = slots.get(&path_key(href)) {
                        report.responses[at].status = Some(404);
                    }
                }
                continue;
            }
            if found.props.get(data).is_none() {
                continue;
            }
            // The member keeps the href the sync named it by, which is the key the store
            // holds it under; a multiget may spell the same resource another way.
            if let Some(&at) = slots.get(&path_key(found.href())) {
                report.responses[at].props = found.props;
            }
        }
    }
    Ok(())
}

/// One multiget. A named function rather than an `async` closure: a closure over the executor
/// is not `Send` for every lifetime the trait's boxed future would need.
async fn fetch(
    exec: &dyn DavExecutor,
    collection: &str,
    body: String,
) -> Result<MultiStatus, CalDavError> {
    exec.send(DavMethod::Report, collection, "1", body)
        .await?
        .into_multistatus()
}

/// Whether two hrefs name the same resource: compared by path, since a server may answer with a
/// path where it was asked about an absolute URL, and with or without a trailing slash.
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    path_key(a) == path_key(b)
}

fn path_key(href: &str) -> String {
    path_of(href).trim_end_matches('/').to_owned()
}

fn path_of(href: &str) -> &str {
    match href.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |at| &rest[at..]),
        None => href,
    }
}

#[cfg(test)]
#[path = "multiget_tests.rs"]
mod tests;
