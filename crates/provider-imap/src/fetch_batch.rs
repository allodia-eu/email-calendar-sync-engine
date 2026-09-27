//! Fetching many messages' raw sources with one `UID FETCH` per mailbox ([`fetch_batch`]).
//!
//! A body read one message at a time pays a round trip per message, and on a first sync of
//! thousands of messages that latency is the whole cost: the bytes themselves arrive in a
//! fraction of it. One `UID FETCH <set> (BODY.PEEK[])` asks for the lot, and the server
//! streams the bodies back to back, so a batch costs one round trip plus its bytes. Each body
//! is handed on as it parses off the wire, never collected, so a batch holds one message in
//! memory at a time.

use std::sync::Arc;

use engine_core::{error::FailureClass, mail::Message, raw::RawMime};
use engine_provider::{ProviderError, SourceStream};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapError,
    mail::parse_message_key,
    pool::{ImapPool, PooledConnection},
    sync::uid_set_spec,
    target::reject_control_chars,
};

/// How many sources one batch request carries, which the provider reports as
/// [`ConnectionInfo::sources_per_request`](engine_provider::ConnectionInfo).
///
/// Large enough that a request's round trip is small beside the bytes it brings back, and small
/// enough that a host spreading batches over the account's connections keeps every one of them
/// busy until the end of a pass, and that a batch cut short by a lost connection asks again for
/// little.
pub(crate) const SOURCES_PER_REQUEST: usize = 25;

/// One message of a batch still waiting for its source.
struct Wanted {
    /// Its position in the caller's slice, which is what the stream reports.
    index: usize,
    validity: u32,
    uid: u32,
}

/// Fetches the sources of `messages` from the account's `pool`, yielding each as it arrives.
///
/// Messages are grouped by mailbox, and each group is one `UID FETCH` on one connection, which
/// is handed one that already has the mailbox open when there is one. A key whose
/// `UIDVALIDITY` no longer matches the mailbox, or a UID the server returns no body for
/// (expunged since the last sync), is a
/// [`Conflict`](engine_core::error::FailureClass::Conflict) for that message alone. A group
/// that lost its connection part-way, or never got one, asks once more for what it had not
/// yet received, on a connection proved alive; after that the rest of the group carries the
/// failure. A group whose response failed for any other reason asks for what it had not yet
/// received one message at a time, so one unreadable message fails alone.
pub(crate) fn fetch_batch<'a, S>(
    pool: &'a Arc<ImapPool<S>>,
    messages: &'a [Message],
) -> SourceStream<'a>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    Box::pin(async_stream::stream! {
        let mut groups: Vec<(&str, Vec<Wanted>)> = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let key = message.id.key().as_str();
            let parsed = parse_message_key(key)
                .filter(|(mailbox, _, _)| reject_control_chars(mailbox).is_ok());
            let Some((mailbox, validity, uid)) = parsed else {
                yield (
                    index,
                    Err(ProviderError::invalid_state(format!("unparseable IMAP message key: {key}"))),
                );
                continue;
            };
            let wanted = Wanted { index, validity, uid };
            match groups.iter_mut().find(|(name, _)| *name == mailbox) {
                Some((_, group)) => group.push(wanted),
                None => groups.push((mailbox, vec![wanted])),
            }
        }

        for (mailbox, mut wanted) in groups {
            let mut retried = false;
            'attempts: loop {
                let acquired = if retried {
                    pool.acquire_checked().await
                } else {
                    pool.acquire_for(mailbox).await
                };
                let mut connection = match acquired {
                    Ok(connection) => connection,
                    Err(err) => {
                        let err = ProviderError::from(err);
                        if again(&mut retried, &err) {
                            continue 'attempts;
                        }
                        for each in wanted.drain(..) {
                            yield (each.index, Err(copy(&err)));
                        }
                        break 'attempts;
                    }
                };
                let open = match open_validity(&mut connection, mailbox).await {
                    Ok(open) => open,
                    Err(err) => {
                        connection.discard();
                        let err = ProviderError::from(err);
                        if again(&mut retried, &err) {
                            continue 'attempts;
                        }
                        for each in wanted.drain(..) {
                            yield (each.index, Err(copy(&err)));
                        }
                        break 'attempts;
                    }
                };
                let (fresh, stale): (Vec<Wanted>, Vec<Wanted>) =
                    wanted.into_iter().partition(|each| each.validity == open);
                wanted = fresh;
                for each in stale {
                    yield (
                        each.index,
                        Err(ProviderError::conflict(format!(
                            "UIDVALIDITY changed for {mailbox}: re-sync before retrying"
                        ))),
                    );
                }
                if wanted.is_empty() {
                    break 'attempts;
                }

                let mut uids: Vec<u32> = wanted.iter().map(|each| each.uid).collect();
                uids.sort_unstable();
                uids.dedup();
                if let Err(err) = connection
                    .uid_fetch_stream_start(&uid_set_spec(&uids), "BODY.PEEK[]")
                    .await
                {
                    connection.discard();
                    let err = ProviderError::from(err);
                    if again(&mut retried, &err) {
                        continue 'attempts;
                    }
                    for each in wanted.drain(..) {
                        yield (each.index, Err(copy(&err)));
                    }
                    break 'attempts;
                }
                loop {
                    match connection.next_fetch_body().await {
                        Ok(Some((uid, bytes))) => {
                            // An unsolicited body for a UID nobody asked for is not ours.
                            let (arrived, rest): (Vec<Wanted>, Vec<Wanted>) =
                                wanted.into_iter().partition(|each| each.uid == uid);
                            wanted = rest;
                            for each in arrived {
                                yield (each.index, Ok(RawMime::new(bytes.clone())));
                            }
                        }
                        Ok(None) => {
                            // The server answered for every UID it still has; the rest were
                            // expunged since the last sync.
                            for each in wanted.drain(..) {
                                yield (
                                    each.index,
                                    Err(ProviderError::conflict(format!(
                                        "message UID {} no longer exists (expunged): re-sync \
                                         before fetching",
                                        each.uid
                                    ))),
                                );
                            }
                            break 'attempts;
                        }
                        Err(err) => {
                            connection.discard();
                            let err = ProviderError::from(err);
                            if again(&mut retried, &err) {
                                continue 'attempts;
                            }
                            // A failure the connection did not cause (a literal over the cap,
                            // a line that does not parse, a `NO`) may be one message's, and
                            // the rest of the set must not carry it: each is asked for alone.
                            if err.class() != FailureClass::Retryable && wanted.len() > 1 {
                                for each in wanted.drain(..) {
                                    let key = messages[each.index].id.key();
                                    let alone = crate::fetch::fetch_from_pool(pool, key).await;
                                    yield (each.index, alone);
                                }
                                break 'attempts;
                            }
                            for each in wanted.drain(..) {
                                yield (each.index, Err(copy(&err)));
                            }
                            break 'attempts;
                        }
                    }
                }
            }
        }
    })
}

/// The `UIDVALIDITY` of `mailbox` on `connection`, opening it read-only unless it is open.
async fn open_validity<S>(
    connection: &mut PooledConnection<S>,
    mailbox: &str,
) -> Result<u32, ImapError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    match connection.open_validity(mailbox) {
        Some(validity) => Ok(validity),
        None => Ok(connection.examine(mailbox).await?.uid_validity),
    }
}

/// Whether to ask once more after `err`: only for a lost connection, and only once.
fn again(retried: &mut bool, err: &ProviderError) -> bool {
    if *retried || err.class() != FailureClass::Retryable {
        return false;
    }
    *retried = true;
    true
}

/// The same failure for another message of the batch; a `ProviderError` carries its source
/// and is not `Clone`, and the class and detail are what a caller reads.
fn copy(err: &ProviderError) -> ProviderError {
    ProviderError::new(err.class(), err.to_string())
}

#[cfg(test)]
#[path = "fetch_batch_tests.rs"]
mod tests;
