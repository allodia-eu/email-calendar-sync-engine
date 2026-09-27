//! Fetching many messages' raw sources in one `Blob/get` (RFC 9404) ([`fetch_batch`]).
//!
//! RFC 8620 downloads one blob per `GET` of the `downloadUrl`, so a body warm pays one round
//! trip per message, and a server's `maxConcurrentRequests` caps how many of those overlap.
//! The blob extension's `Blob/get` returns many blobs' bytes in one method call, as base64,
//! so a batch costs one round trip. Base64 carries a third more bytes than the download, which
//! for a large message costs more than the round trip it saves: a message over
//! [`INLINE_MAX`] is downloaded on its own, as before.

use std::collections::HashMap;

use engine_core::{error::FailureClass, mail::Message, raw::RawMime};
use engine_provider::{ProviderError, SourceStream};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// How many sources one batch request carries, reported as
/// [`ConnectionInfo::sources_per_request`](engine_provider::ConnectionInfo) when the account
/// answers `Blob/get`.
///
/// Large enough that a call's round trip is small beside the bytes it brings back, small
/// enough that a host overlapping batches keeps every request slot busy until the end of a
/// pass.
pub(crate) const SOURCES_PER_REQUEST: usize = 25;

/// The largest message, in octets, read inline through `Blob/get`; anything larger, or of
/// unknown size, is downloaded on its own.
///
/// The trade is a third more bytes against one round trip. The round trip is already
/// overlapped with the account's other requests, so what it costs is roughly the latency over
/// the request width: about 20 ms on a server answering in 200 ms with ten in flight. Over a
/// 200 Mbps link that is the time half a megabyte takes, which a message of 1.5 MiB spends in
/// base64 overhead. Below this size the batch wins.
pub(crate) const INLINE_MAX: u64 = 1024 * 1024;

/// The most bytes of messages one `Blob/get` asks for, so a response stays a few megabytes of
/// JSON however the sizes fall.
const INLINE_BYTES_PER_CALL: u64 = 8 * 1024 * 1024;

/// Fetches the sources of `messages`, yielding each as it arrives.
///
/// Messages small enough ([`INLINE_MAX`]) are read in as few `Blob/get` calls as the server's
/// `maxObjectsInGet` and [`INLINE_BYTES_PER_CALL`] allow; the rest are downloaded one at a
/// time. A blob the server reports `notFound` fails that message alone, as a download of it
/// would. A blob the call returned without its data (truncated, or answered neither way) is
/// downloaded instead, as are the large messages, overlapped. A call the server refused for any
/// reason but load falls back to
/// downloading its messages; one refused for load, or lost to the transport, fails them, so
/// the host tries again on its next pass rather than adding requests to a server that asked
/// for fewer.
pub(crate) fn fetch_batch<'a>(
    executor: &'a dyn Executor,
    messages: &'a [Message],
) -> SourceStream<'a> {
    Box::pin(async_stream::stream! {
        let mut calls: Vec<Vec<(usize, &str)>> = Vec::new();
        let mut alone = Vec::new();
        let most = executor.session().limits().max_objects_in_get.max(1);
        let mut bytes = 0u64;
        for (index, message) in messages.iter().enumerate() {
            let inline = message
                .blob_id
                .as_ref()
                .zip(message.size.filter(|size| *size <= INLINE_MAX));
            let Some((blob, size)) = inline else {
                alone.push(index);
                continue;
            };
            let full = calls
                .last()
                .is_none_or(|call| call.len() >= most || bytes + size > INLINE_BYTES_PER_CALL);
            if full {
                calls.push(Vec::new());
                bytes = 0;
            }
            bytes += size;
            calls
                .last_mut()
                .expect("a call was just opened")
                .push((index, blob.as_str()));
        }

        for call in calls {
            match blob_get(executor, &call).await {
                Ok(mut found) => {
                    for (index, _blob) in call {
                        match found.remove(&index) {
                            Some(outcome) => yield (index, outcome),
                            None => alone.push(index),
                        }
                    }
                }
                Err(err) if held_back(&err) => {
                    for (index, _blob) in call {
                        yield (index, Err(ProviderError::new(err.class(), err.to_string())));
                    }
                }
                Err(_) => alone.extend(call.into_iter().map(|(index, _blob)| index)),
            }
        }

        // Overlapped, so a batch holding a few large messages does not hold its request slot
        // for their sum. The account's request gate bounds how many of these and every other
        // request are in flight together, so this cannot go past what the server allows.
        let width = executor.session().limits().max_concurrent_requests.max(1);
        let mut downloads = futures_util::stream::iter(alone)
            .map(|index| async move {
                let one = crate::blob::message_source(executor, &messages[index]).await;
                (index, one.map_err(ProviderError::from))
            })
            .buffer_unordered(width);
        while let Some(outcome) = downloads.next().await {
            yield outcome;
        }
    })
}

/// Whether a failed call must not be answered with more requests: the server asked for fewer,
/// or the transport is failing anyway.
fn held_back(err: &ProviderError) -> bool {
    matches!(
        err.class(),
        FailureClass::RateLimited | FailureClass::Retryable | FailureClass::Authentication
    )
}

/// One `Blob/get` for `wanted`, mapping each position in the caller's slice to its outcome.
/// A position absent from the result was returned without usable data, and is the caller's
/// to download.
async fn blob_get(
    executor: &dyn Executor,
    wanted: &[(usize, &str)],
) -> Result<HashMap<usize, Result<RawMime, ProviderError>>, ProviderError> {
    let account = executor.session().mail_account_id()?;
    let ids: Vec<&str> = wanted.iter().map(|(_, blob)| *blob).collect();
    let mut request = Request::new([capability::CORE, capability::BLOB]);
    let call = request.invoke(
        "Blob/get",
        json!({
            "accountId": account,
            "ids": ids,
            "properties": ["data:asBase64", "size"],
        }),
    );
    let response = executor.execute(&request).await?;
    let result = response.result(&call)?;

    let mut data: HashMap<&str, RawMime> = HashMap::new();
    for item in result
        .get("list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let whole = !item
            .get("isTruncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let bytes = item
            .get("data:asBase64")
            .and_then(Value::as_str)
            .filter(|_| whole)
            .and_then(crate::base64::decode);
        let size = item.get("size").and_then(Value::as_u64);
        // A size that disagrees with the bytes means the data is not the whole blob, whatever
        // the flags say; a download of it is the only answer that can be trusted.
        let complete = |bytes: &Vec<u8>| {
            size.is_none_or(|size| u64::try_from(bytes.len()).is_ok_and(|len| len == size))
        };
        if let Some(bytes) = bytes.filter(complete) {
            data.insert(id, RawMime::new(bytes));
        }
    }
    let gone: Vec<&str> = result
        .get("notFound")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let mut outcomes = HashMap::new();
    // Taken, not cloned: a body is megabytes, and copying every one to hand it on would double
    // the warm's memory traffic. A blob asked for twice finds its data gone the second time and
    // is downloaded instead, which a batch built from distinct messages never does.
    for (index, blob) in wanted {
        if let Some(raw) = data.remove(blob) {
            outcomes.insert(*index, Ok(raw));
        } else if gone.contains(blob) {
            let err = JmapError::protocol(format!("blob {blob} not found"));
            outcomes.insert(
                *index,
                Err(ProviderError::new(FailureClass::Permanent, err.to_string())),
            );
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
#[path = "blob_batch_tests.rs"]
mod tests;
