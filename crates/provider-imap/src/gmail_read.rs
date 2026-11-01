//! The commands behind [`crate::GmailMail`], each over one connection so the offline suite can
//! replay them on a scripted stream.
//!
//! Every read names the folder's `UIDVALIDITY` it was planned against and checks it before
//! sending anything: a UID means nothing under another validity, and a caller that kept going
//! would file one message's state under another's id.

use engine_core::ids::MailboxId;
use engine_provider::{ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult,
    gmail::{GmailFolderState, GmailMessage, GmailQuery, GmailState},
    mail::message_from_fetch,
    metadata_fetch::FETCH_ITEMS,
    parse::FetchRow,
    sync::uid_set_spec,
    transport::Connection,
    transport_command::{format_imap_date, quote},
};

/// What a state read asks for: enough to know where a message is filed and what it is marked.
const STATE_ITEMS: &str = "UID FLAGS X-GM-MSGID X-GM-THRID X-GM-LABELS";

/// The Gmail items a metadata read adds to [`FETCH_ITEMS`].
const GMAIL_ITEMS: &str = "X-GM-MSGID X-GM-THRID X-GM-LABELS";

/// UIDs per metadata `FETCH`, lowered to a server's `MESSAGELIMIT` where it states one.
const FETCH_BATCH: usize = 500;

/// Gmail ids per body search: about 1.8 KB of command, far under RFC 7162 §4's 8192 octets.
pub(crate) const SOURCE_BATCH: usize = 50;

/// UIDs one command names when they are listed rather than fetched in full: at worst about 7 KB
/// of UID set, under RFC 7162 §4's 8192 octets however scattered the UIDs are.
const ID_BATCH: usize = 1000;

/// `SELECT (CONDSTORE)` of `name`, reporting the counters a pass plans from.
pub(crate) async fn open<S>(
    connection: &mut Connection<S>,
    name: &str,
) -> ImapResult<GmailFolderState>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let data = connection.select_condstore(name).await?;
    Ok(GmailFolderState {
        uid_validity: data.uid_validity,
        uid_next: data.uid_next.unwrap_or(1),
        highest_modseq: data.highest_modseq.unwrap_or(0),
        exists: data.exists,
    })
}

/// Opens `name` unless this session already has it open under `validity`, and refuses when
/// the folder has been renumbered since the caller planned against it.
pub(crate) async fn ensure_open<S>(
    connection: &mut Connection<S>,
    name: &str,
    validity: u32,
) -> ProviderResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if connection.open_validity(name) == Some(validity) {
        return Ok(());
    }
    let now = open(connection, name).await?.uid_validity;
    if now == validity {
        Ok(())
    } else {
        Err(ProviderError::needs_resync(format!(
            "the folder was renumbered (UIDVALIDITY {validity} is now {now}): sync it afresh"
        )))
    }
}

/// `UID SEARCH` for `query` in the open folder, the UIDs ascending. A long `uids` list is asked
/// about [`ID_BATCH`] at a time.
pub(crate) async fn search<S>(
    connection: &mut Connection<S>,
    query: &GmailQuery,
) -> ImapResult<Vec<u32>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut found = Vec::new();
    match &query.uids {
        Some(uids) => {
            for chunk in sorted(uids).chunks(ID_BATCH) {
                found.extend(search_once(connection, query, Some(chunk)).await?);
            }
        }
        None => found = search_once(connection, query, None).await?,
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

async fn search_once<S>(
    connection: &mut Connection<S>,
    query: &GmailQuery,
    uids: Option<&[u32]>,
) -> ImapResult<Vec<u32>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut criteria = Vec::new();
    if let Some(uids) = uids {
        criteria.push(format!("UID {}", uid_set_spec(uids)));
    }
    if let Some(from) = query.from_uid {
        criteria.push(format!("UID {from}:*"));
    }
    if let Some(since) = query.since {
        criteria.push(format!("SINCE {}", format_imap_date(since)));
    }
    if let Some(raw) = &query.raw {
        criteria.push(format!("X-GM-RAW {}", quote(raw)));
    }
    if criteria.is_empty() {
        criteria.push("ALL".to_owned());
    }
    let found = connection.uid_search(&criteria.join(" ")).await?;
    // `n:*` names the highest UID even when every UID is below `n` (RFC 9051 §6.4.8).
    Ok(found
        .into_iter()
        .filter(|uid| query.from_uid.is_none_or(|from| *uid >= from))
        .collect())
}

/// The whole metadata of `uids` in the open folder, attachment flags settled.
pub(crate) async fn messages<S>(
    connection: &mut Connection<S>,
    folder: &MailboxId,
    validity: u32,
    uids: &[u32],
) -> ImapResult<Vec<GmailMessage>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let items = format!("{FETCH_ITEMS} {GMAIL_ITEMS}");
    let utf7 = connection.negotiated.names_are_modified_utf7();
    let mut out = Vec::with_capacity(uids.len());
    for set in batches(
        uids,
        connection.negotiated.within_message_limit(FETCH_BATCH),
    ) {
        let mut rows = connection.uid_fetch(&set, &items).await?;
        connection.settle_attachments(&mut rows).await?;
        out.extend(rows.into_iter().filter_map(|row| {
            let message = message_from_fetch(&row, folder, validity);
            let date = row
                .envelope
                .as_ref()
                .and_then(|envelope| envelope.date.clone());
            state_of(row, utf7).map(|state| GmailMessage {
                state,
                message,
                date,
            })
        }));
    }
    Ok(out)
}

/// The state of every message below `below_uid` whose mod-sequence is above `modseq`.
pub(crate) async fn changed<S>(
    connection: &mut Connection<S>,
    below_uid: u32,
    modseq: u64,
) -> ImapResult<Vec<GmailState>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if below_uid <= 1 {
        return Ok(Vec::new());
    }
    let utf7 = connection.negotiated.names_are_modified_utf7();
    let set = format!("1:{}", below_uid - 1);
    let rows = connection
        .uid_fetch_changed(&set, STATE_ITEMS, modseq)
        .await?;
    // `1:n` names the highest UID even when every UID is above `n` (RFC 9051 §6.4.8).
    Ok(rows
        .into_iter()
        .filter(|row| row.uid < below_uid)
        .filter_map(|row| state_of(row, utf7))
        .collect())
}

/// The Gmail id of each of `uids` in the open folder, as `(uid, id)`.
pub(crate) async fn ids<S>(
    connection: &mut Connection<S>,
    uids: &[u32],
) -> ImapResult<Vec<(u32, u64)>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut out = Vec::with_capacity(uids.len());
    for set in batches(uids, connection.negotiated.within_message_limit(ID_BATCH)) {
        for row in connection.uid_fetch(&set, "UID X-GM-MSGID").await? {
            if let Some(id) = row.gmail.message {
                out.push((row.uid, id));
            }
        }
    }
    Ok(out)
}

/// Where the open folder holds each of `ids` (at most [`SOURCE_BATCH`]), as `(uid, id)`; an id
/// the folder does not hold is left out.
pub(crate) async fn located<S>(
    connection: &mut Connection<S>,
    ids: &[u64],
) -> ImapResult<Vec<(u32, u64)>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let found = connection.uid_search(&any_of(ids)).await?;
    if found.is_empty() {
        return Ok(Vec::new());
    }
    Ok(connection
        .uid_fetch(&uid_set_spec(&sorted(&found)), "UID X-GM-MSGID")
        .await?
        .into_iter()
        .filter_map(|row| row.gmail.message.map(|id| (row.uid, id)))
        .collect())
}

/// `OR X-GM-MSGID a OR X-GM-MSGID b X-GM-MSGID c`: a search for any of `ids`.
fn any_of(ids: &[u64]) -> String {
    let mut criteria = "OR ".repeat(ids.len().saturating_sub(1));
    let each: Vec<String> = ids.iter().map(|id| format!("X-GM-MSGID {id}")).collect();
    criteria.push_str(&each.join(" "));
    criteria
}

/// The Gmail half of a row; `None` for a row without its Gmail id, which a server offering
/// `X-GM-EXT-1` always sends when asked.
fn state_of(row: FetchRow, modified_utf7: bool) -> Option<GmailState> {
    let gmail_id = row.gmail.message?;
    let labels = row
        .gmail
        .labels
        .unwrap_or_default()
        .into_iter()
        .map(|label| {
            if modified_utf7 && !label.starts_with('\\') {
                crate::utf7::decode(&label)
            } else {
                label
            }
        })
        .collect();
    Some(GmailState {
        uid: row.uid,
        gmail_id,
        thread_id: row.gmail.thread.unwrap_or(gmail_id),
        labels,
        flags: row.flags,
        modseq: row.modseq,
    })
}

/// `uids` as `UID` sets of at most `limit` UIDs each (`0` for one set), highest first so the
/// newest mail is read first.
fn batches(uids: &[u32], limit: usize) -> Vec<String> {
    let mut ordered = sorted(uids);
    ordered.reverse();
    let size = if limit == 0 {
        ordered.len().max(1)
    } else {
        limit
    };
    ordered
        .chunks(size)
        .map(|chunk| {
            let mut chunk = chunk.to_vec();
            chunk.sort_unstable();
            uid_set_spec(&chunk)
        })
        .collect()
}

fn sorted(uids: &[u32]) -> Vec<u32> {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

#[cfg(test)]
#[path = "gmail_read_tests.rs"]
mod tests;
