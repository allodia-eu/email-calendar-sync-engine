//! How a batch travels: which messages go up inside the request (RFC 9404 `Blob/upload`,
//! referenced as `#b<n>` from `Email/import`) and which by the `uploadUrl`, and how they are
//! packed into requests the server takes.
//!
//! Inline is the default wherever it fits, and on Stalwart it is not an optimisation: the server
//! takes about 1,000 requests per account a minute, and one upload per message met that wall
//! within seconds where inline batches did not (`jmap.md` → "Import"). A message goes by URL only
//! when it cannot go inline: the account lacks the blob extension, the message is larger than
//! one `Blob/upload` takes (`maxSizeBlobSet`), or its base64 alone would not fit a request.
//!
//! A request is sized in the bytes it will be on the wire, base64 included, against
//! `maxSizeRequest`: a request over it fails whole (a `400`, `limit: maxSizeRequest`). Each
//! method call also takes at most `maxObjectsInSet` objects.

use crate::{session::CoreLimits, session_limits::BlobUpload};

/// What a request spends before its first message: the envelope, the two method names and call
/// ids, the capability URNs, and the account id twice (added per account by [`Budget::new`]).
const ENVELOPE: u64 = 384;

/// What one inline blob spends beside its base64, its creation id included:
/// `"b4294967295":{"data":[{"data:asBase64":""}],"type":"message/rfc822"},`.
const BLOB_ENTRY: u64 = 80;

/// The longest reference to an inline blob, `#b4294967295`.
const INLINE_REF: u64 = 12;

/// The longest blob id a server may name (RFC 8620 §1.2): what an entry referencing a blob by
/// URL reserves, since it is sized before the upload that names the id.
const LONGEST_ID: u64 = 255;

/// The bytes `len` bytes take in standard base64, padding included (RFC 4648 §4).
pub(crate) fn base64_len(len: usize) -> u64 {
    (len as u64).div_ceil(3) * 4
}

/// How one message travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Inside the request, through `Blob/upload`.
    Inline,
    /// To the `uploadUrl` first, then referenced by its blob id.
    ByUrl,
}

/// One message as the packer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Item {
    /// How it travels.
    pub(crate) route: Route,
    /// Its size in bytes.
    pub(crate) len: usize,
    /// The bytes of its `Email/import` entry, creation id included and blob id left out.
    pub(crate) entry: u64,
}

impl Item {
    /// The bytes it adds to a request.
    fn cost(&self) -> u64 {
        self.entry
            + match self.route {
                Route::Inline => INLINE_REF + base64_len(self.len) + BLOB_ENTRY,
                Route::ByUrl => LONGEST_ID,
            }
    }
}

/// What a request may hold, from the session.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Bytes left for messages once the envelope is paid.
    bytes: u64,
    /// Messages per method call.
    objects: usize,
    /// The largest blob one `Blob/upload` takes, or `None` where none goes inline.
    inline: Option<u64>,
}

impl Budget {
    /// The budget of requests into an account named `account_len` bytes long.
    pub(crate) fn new(limits: &CoreLimits, blobs: BlobUpload, account_len: usize) -> Self {
        let envelope = ENVELOPE + 2 * account_len as u64;
        // Inline takes two calls, `Blob/upload` and the `Email/import` that references it.
        let inline = match blobs {
            BlobUpload::Supported { max_size } if limits.max_calls_in_request >= 2 => {
                Some(max_size.unwrap_or(u64::MAX))
            }
            _ => None,
        };
        Self {
            bytes: limits.max_size_request.saturating_sub(envelope),
            objects: limits.max_objects_in_set.max(1),
            inline,
        }
    }

    /// The [`Item`] a message of `len` bytes, whose `Email/import` entry is `entry` bytes without
    /// its blob id, is:
    /// inline where the account takes blobs in a request, the blob is not larger than one
    /// `Blob/upload` takes, and it fits a request on its own.
    pub(crate) fn item(&self, len: usize, entry: u64) -> Item {
        let inline = Item {
            route: Route::Inline,
            len,
            entry,
        };
        let fits = self
            .inline
            .is_some_and(|largest| len as u64 <= largest && inline.cost() <= self.bytes);
        if fits {
            inline
        } else {
            Item {
                route: Route::ByUrl,
                ..inline
            }
        }
    }

    /// Packs `items` into requests, in order: each request is the positions of the items it
    /// carries. Every request is within the byte and object budgets, unless one by-URL entry is
    /// larger than a whole request, which then travels alone.
    pub(crate) fn pack(&self, items: &[Item]) -> Vec<Vec<usize>> {
        let mut requests = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        let mut spent = 0;
        for (at, item) in items.iter().enumerate() {
            let cost = item.cost();
            if !current.is_empty() && (current.len() == self.objects || spent + cost > self.bytes) {
                requests.push(std::mem::take(&mut current));
                spent = 0;
            }
            current.push(at);
            spent += cost;
        }
        if !current.is_empty() {
            requests.push(current);
        }
        requests
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
