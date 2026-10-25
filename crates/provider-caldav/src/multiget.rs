//! Fetching the bodies a `sync-collection` answer left out.
//!
//! RFC 6578 §3.2 lets a server answer a sync with fewer properties than were asked for, and
//! iCloud does so for every member: each changed resource arrives with its `getetag` and without
//! its `calendar-data` or `address-data`. Such a member is fetched again with a multiget
//! (RFC 4791 §7.9, RFC 6352 §8.7), and its answer takes the bodiless one's place, so what follows
//! reads every changed member the same way whichever server sent it.

use std::collections::HashMap;

use crate::{
    dav::MultiStatus,
    error::CalDavError,
    transport::{DavExecutor, DavMethod},
};

/// How many members one multiget names: enough that a collection of thousands costs tens of
/// requests on its first sync, few enough that no answer is unreasonably large.
const BATCH: usize = 100;

/// Fetches, with `multiget`, the body of every changed member of `report` that arrived without
/// its `data` property, and puts each answer in place of the member it fetched.
///
/// The collection's own entry is not a member and is left as it is. A member the multiget does
/// not return with a body is left as it came, which is what the caller already makes of a body
/// it cannot read.
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
    for batch in missing.chunks(BATCH) {
        let fetched = exec
            .send(DavMethod::Report, collection, "1", multiget(batch))
            .await?
            .into_multistatus()?;
        for found in fetched.responses {
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
