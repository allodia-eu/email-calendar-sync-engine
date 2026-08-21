//! Label-list, message snapshot/history-delta fetch + paging, and raw-source fetch for
//! the Gmail provider.
//!
//! Gmail's sync is **account-global** (`historyId`), not per-folder like Graph, so there
//! are two shapes rather than one unified delta:
//!
//! - **Snapshot** ([`snapshot_page`], `cursor` `None`): `users.messages.list` enumerates `{id,
//!   threadId}` refs (paginated by `pageToken`, optionally windowed by a `q: after:<epoch>` floor);
//!   each id is fetched full (`messages.get?format=metadata`, [`MAX_CONCURRENT_GETS`] at a time)
//!   and normalized. The cursor to persist is the account `historyId` captured *before* the
//!   enumeration (any messages that arrive mid-snapshot are simply re-reported by the first delta —
//!   idempotent).
//! - **Delta** ([`delta_page`], `cursor` `Some`): `users.history.list?startHistoryId=…` returns
//!   `messagesAdded`/`labelsAdded`/`labelsRemoved` (whose message objects are *partials* — id +
//!   labelIds only) and `messagesDeleted`. Deleted ids tombstone. A partial's `labelIds` is the
//!   message's **resulting** label set, and in Gmail that set is the whole of a message's mutable
//!   state — its keywords *and* its filing — so any label change (a mark-read, a star, an archive,
//!   which in Gmail is a label change like any other) is already answered by the page and becomes a
//!   state change costing no further request. Only `messagesAdded` is re-fetched full, because
//!   nothing in a history record carries a subject, a sender or a body. A `404` (the
//!   `startHistoryId` aged out of the window) becomes [`GoogleError::HistoryExpired`] → the stream
//!   restarts as a snapshot.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    ids::ProviderKey,
    mail::{MailState, MailStateChange, Mailbox, Message},
    raw::RawMime,
    sync::SyncState,
    time::CalendarDate,
};
use engine_provider::{PageToken, SyncKind, SyncPage};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::Value;

use crate::{
    base64url,
    error::GoogleError,
    json::{opt_str, req_str},
    normalize::{
        METADATA_HEADERS, all_mail_mailbox, keywords_from_labels, label_from_json, memberships_of,
        message_from_json,
    },
    transport::{GoogleClient, encode_query_value},
};

/// The Gmail user-scoped API root (`/gmail/v1/users/me`).
const USERS_ME: &str = "/gmail/v1/users/me";

/// How many `messages.get` calls a page keeps in flight.
///
/// `messages.list` returns only `{id, threadId}`, so every message costs its own request and
/// a page drained one at a time is as many round trips deep as it has messages — Gmail is the
/// only adapter here whose list endpoint has no companion batch-get to hide that behind, and
/// the per-request cost is Google's own service latency rather than the network's, so a fast
/// link does not shorten it.
///
/// Gmail answers `429` past **50 concurrent requests per mailbox** whatever the quota allows.
/// Measured against a live account, 20 is the widest window that stays clean: 30 draws the
/// occasional throttle and 50 throttles a tenth of its requests. Throughput scales with the
/// window because each round costs one round trip whatever its width, so this is a ceiling
/// question, not a tuning one.
///
/// The batch endpoint is deliberately not used. It is no way around the limits — Google
/// documents a batch of n as counting for n requests, and batches do throttle — and at equal
/// width it is not measurably faster either: both shapes cost one round trip per round, and
/// repeated runs hand the win to whichever side the network favoured that minute. What it
/// reliably does cost is about a quarter more bytes for the multipart envelope, and a parser:
/// a batch answers `200` while individual members carry their own `429`, so every
/// sub-response needs its status read and its retry driven separately. Concurrency gets the
/// same throughput from the HTTP/2 connection the client already pools, and can hand a
/// message on as it lands rather than after a whole envelope parses.
/// `tests/live_batch_vs_concurrent.rs` is the gated probe behind all of that.
pub(crate) const MAX_CONCURRENT_GETS: usize = 20;

/// Fetches the account's labels as mailboxes, dropping the keyword-only labels
/// (`UNREAD`/`STARRED`) and appending the synthetic All Mail home.
pub(crate) async fn labels(client: &GoogleClient) -> Result<Vec<Mailbox>, GoogleError> {
    let doc = client
        .get(&client.url(&format!("{USERS_ME}/labels")))
        .await?;
    let mut mailboxes = Vec::new();
    for label in array(&doc, "labels", "labels")? {
        if let Some(mailbox) = label_from_json(label)? {
            mailboxes.push(mailbox);
        }
    }
    mailboxes.push(all_mail_mailbox());
    Ok(mailboxes)
}

/// Fetches the current account-global `historyId` (the snapshot's delta cursor).
pub(crate) async fn current_history_id(client: &GoogleClient) -> Result<SyncState, GoogleError> {
    let doc = client
        .get(&client.url(&format!("{USERS_ME}/profile")))
        .await?;
    Ok(SyncState::new(req_str(&doc, "historyId")?))
}

/// Fetches one message's raw RFC 5322 source (`messages.get?format=raw`), decoding the
/// base64url `raw` field. `key` is the Gmail message id.
pub(crate) async fn message_source(
    client: &GoogleClient,
    key: &ProviderKey,
) -> Result<RawMime, GoogleError> {
    let doc = client
        .get(&client.url(&format!("{USERS_ME}/messages/{}?format=raw", key.as_str())))
        .await?;
    let raw = req_str(&doc, "raw")?;
    let bytes = base64url::decode(raw)
        .ok_or_else(|| GoogleError::protocol("messages.get raw was not valid base64url"))?;
    Ok(RawMime::new(bytes))
}

/// Fetches just a message's current `labelIds` (`format=minimal`) — the current place
/// set a `MoveTo` replacement needs to compute what to remove.
pub(crate) async fn message_labels(
    client: &GoogleClient,
    key: &ProviderKey,
) -> Result<Vec<String>, GoogleError> {
    let doc = client
        .get(&client.url(&format!(
            "{USERS_ME}/messages/{}?format=minimal",
            key.as_str()
        )))
        .await?;
    Ok(doc
        .get("labelIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

/// Fetches one message full (`format=metadata`, the envelope headers this adapter reads)
/// and normalizes it.
async fn get_message(client: &GoogleClient, id: &str) -> Result<Message, GoogleError> {
    use std::fmt::Write as _;
    let mut path = format!("{USERS_ME}/messages/{id}?format=metadata");
    for header in METADATA_HEADERS {
        let _ = write!(path, "&metadataHeaders={header}");
    }
    let doc = client.get(&client.url(&path)).await?;
    message_from_json(&doc)
}

/// Fetches every id full, up to [`MAX_CONCURRENT_GETS`] at a time, in the order `ids` gave.
///
/// A `404` is a message deleted in the race between the enumeration and its fetch: it drops
/// out of the result rather than failing the page, and a later delta reports the removal.
/// Ordered rather than completion-ordered so a page's rows land in the order the server
/// listed them however the individual fetches interleave.
async fn get_messages(
    client: &GoogleClient,
    ids: Vec<String>,
) -> Result<Vec<Message>, GoogleError> {
    stream::iter(ids)
        .map(|id| async move { get_message(client, &id).await })
        .buffered(MAX_CONCURRENT_GETS)
        .filter_map(|result| async move {
            match result {
                Err(GoogleError::Status { status: 404, .. }) => None,
                other => Some(other),
            }
        })
        .try_collect()
        .await
}

/// Fetches one snapshot page: a `messages.list` page whose ids are fetched full, concurrently.
/// `floor` (the sync-window date floor) windows the enumeration to `after:<floor>`;
/// `history_id` is the account cursor captured before the snapshot, carried as the
/// page's `next_cursor`.
pub(crate) async fn snapshot_page(
    client: &GoogleClient,
    page: Option<&PageToken>,
    floor: Option<CalendarDate>,
    history_id: &SyncState,
) -> Result<SyncPage<Message>, GoogleError> {
    let doc = client.get(&list_url(client, page, floor)).await?;

    // `messages` is absent on an empty mailbox / final empty page — treat as no rows.
    let entries = doc.get("messages").and_then(Value::as_array);
    let mut ids = Vec::new();
    for entry in entries.into_iter().flatten() {
        ids.push(req_str(entry, "id")?.to_owned());
    }
    let changed = get_messages(client, ids).await?;
    let present = changed.iter().map(|m| m.id.key().clone()).collect();

    Ok(SyncPage {
        kind: SyncKind::Snapshot,
        changed,
        patched: Vec::new(),
        removed: Vec::new(),
        present,
        next_page: opt_str(&doc, "nextPageToken").map(PageToken::new),
        next_cursor: history_id.clone(),
        total: None,
    })
}

/// Fetches one history-delta page from `cursor` (a `historyId`): re-fetches each message
/// whose content this page could not already know, turns the label-only changes into
/// state changes, and tombstones each deleted one. A `404` (aged-out cursor) becomes
/// [`GoogleError::HistoryExpired`].
pub(crate) async fn delta_page(
    client: &GoogleClient,
    cursor: &SyncState,
    page: Option<&PageToken>,
) -> Result<SyncPage<Message>, GoogleError> {
    let doc = match client.get(&history_url(client, cursor, page)).await {
        Ok(doc) => doc,
        Err(GoogleError::Status {
            status: 404, body, ..
        }) => {
            return Err(GoogleError::history_expired(body));
        }
        Err(other) => return Err(other),
    };

    let history = collect_history(&doc)?;
    // A message changed then deleted inside the same window 404s here and drops out; the
    // page's own tombstone covers it.
    let changed = get_messages(client, history.refetch).await?;

    // Gmail returns the latest historyId even when nothing changed, so the cursor always
    // advances; fall back to the prior cursor only if the field is somehow absent.
    let next_cursor = opt_str(&doc, "historyId").map_or_else(|| cursor.clone(), SyncState::new);
    Ok(SyncPage {
        kind: SyncKind::Delta,
        changed,
        patched: history.patched,
        removed: history.removed,
        present: Vec::new(),
        next_page: opt_str(&doc, "nextPageToken").map(PageToken::new),
        next_cursor,
        total: None,
    })
}

/// What one history page asks of the adapter.
struct HistoryPage {
    /// Ids whose whole object must be re-fetched — a new arrival, or a message whose
    /// *filing* moved. Strings, because they address the re-fetch `get`.
    refetch: Vec<String>,
    /// Label-only changes, which the page has already answered in full.
    patched: Vec<MailStateChange>,
    /// Tombstones.
    removed: Vec<ProviderKey>,
}

/// Sorts a history page into the three.
///
/// A `labelsAdded`/`labelsRemoved` record carries the message's **resulting** `labelIds` in
/// full, and in Gmail that set is the whole of a message's mutable state: labels are both its
/// keywords (`UNREAD`, `STARRED`) and its filing (`INBOX`, and every folder-like label). So any
/// label change — a mark-read, a star, an archive — is answered by the page itself, with no
/// further request. Only `messagesAdded` needs a fetch, because nothing here carries a subject,
/// a sender or a body.
///
/// A message added-then-deleted in the same window is only tombstoned.
fn collect_history(doc: &Value) -> Result<HistoryPage, GoogleError> {
    let records = doc.get("history").and_then(Value::as_array);

    // Deletions win, so gather them before deciding anything else.
    let mut removed = Vec::new();
    let mut deleted = BTreeSet::new();
    for record in records.into_iter().flatten() {
        for id in message_ids(record, "messagesDeleted") {
            if deleted.insert(id.clone()) {
                removed.push(
                    ProviderKey::new(&id)
                        .map_err(|e| GoogleError::protocol(format!("bad deleted id: {e}")))?,
                );
            }
        }
    }

    let mut order: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut whole: BTreeSet<String> = BTreeSet::new();
    let mut resulting: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for record in records.into_iter().flatten() {
        for group in ["messagesAdded", "labelsAdded", "labelsRemoved"] {
            for entry in group_entries(record, group) {
                let Some(id) = entry.get("message").and_then(|m| opt_str(m, "id")) else {
                    continue;
                };
                let id = id.to_owned();
                if deleted.contains(&id) {
                    continue;
                }
                if seen.insert(id.clone()) {
                    order.push(id.clone());
                }
                if group == "messagesAdded" {
                    // New to us: nothing here carries a subject, sender or body.
                    whole.insert(id);
                    continue;
                }
                // The set the change left behind. Absent means the page cannot answer the
                // change on its own.
                let Some(result) = resulting_labels(entry) else {
                    whole.insert(id);
                    continue;
                };
                // Records arrive in ascending historyId order, so a later one is the
                // more recent word on the same message.
                resulting.insert(id, result);
            }
        }
    }

    let mut page = HistoryPage {
        refetch: Vec::new(),
        patched: Vec::new(),
        removed,
    };
    for id in order {
        match resulting.get(&id) {
            Some(labels) if !whole.contains(&id) => {
                let key = ProviderKey::new(&id)
                    .map_err(|e| GoogleError::protocol(format!("bad changed id: {e}")))?;
                let state = MailState::with_keywords(keywords_from_labels(labels))
                    .filed_in(memberships_of(labels)?);
                page.patched.push(MailStateChange::new(key, state));
            }
            _ => page.refetch.push(id),
        }
    }
    Ok(page)
}

/// The entries of a history record's `group` array (e.g. `messagesAdded`).
fn group_entries<'a>(record: &'a Value, group: &str) -> impl Iterator<Item = &'a Value> {
    record
        .get(group)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The `message.id`s inside a history record's `group` array.
fn message_ids(record: &Value, group: &str) -> Vec<String> {
    group_entries(record, group)
        .filter_map(|entry| entry.get("message").and_then(|m| opt_str(m, "id")))
        .map(str::to_owned)
        .collect()
}

/// The resulting label set carried on the entry's `message`.
///
/// `None` when the field is **absent**, which is not the same as empty: an empty set is a
/// read, archived, unstarred message, and [`keywords_from_labels`] reads the absence of
/// `UNREAD` as `$seen`. Treating a missing field as empty would mark unread mail read.
fn resulting_labels(entry: &Value) -> Option<Vec<String>> {
    string_array(entry.get("message").and_then(|m| m.get("labelIds")))
}

/// A JSON array of strings as owned values; `None` when the field is absent or not an
/// array.
fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

/// The `messages.list` URL: a continuation `pageToken`, else the first page, optionally
/// windowed by an `after:<epoch>` floor.
///
/// `includeSpamTrash` is not optional here. `messages.list` omits both by default while
/// `history.list` reports their label changes regardless — it takes no such flag — so
/// without it a delta files a message into Junk and the next snapshot, seeing it in no
/// page, tombstones it. Which one the store believes would depend on whether the last
/// pass happened to be a snapshot.
fn list_url(
    client: &GoogleClient,
    page: Option<&PageToken>,
    floor: Option<CalendarDate>,
) -> String {
    use std::fmt::Write as _;
    let mut url = format!("{USERS_ME}/messages?maxResults=100&includeSpamTrash=true");
    if let Some(page) = page {
        let _ = write!(url, "&pageToken={}", encode_query_value(page.as_str()));
    }
    if let Some(epoch) = floor.and_then(midnight_epoch) {
        let _ = write!(url, "&q=after:{epoch}");
    }
    client.url(&url)
}

/// The `history.list` URL from `cursor` (a `startHistoryId`), optionally continued.
fn history_url(client: &GoogleClient, cursor: &SyncState, page: Option<&PageToken>) -> String {
    use std::fmt::Write as _;
    let mut url = format!(
        "{USERS_ME}/history?startHistoryId={}",
        encode_query_value(cursor.as_str())
    );
    if let Some(page) = page {
        let _ = write!(url, "&pageToken={}", encode_query_value(page.as_str()));
    }
    client.url(&url)
}

/// The Unix-epoch seconds for `date` at 00:00:00 UTC (the `q: after:` window bound).
fn midnight_epoch(date: CalendarDate) -> Option<i64> {
    let month = time::Month::try_from(date.month()).ok()?;
    let day = time::Date::from_calendar_date(date.year(), month, date.day()).ok()?;
    Some(day.midnight().assume_utc().unix_timestamp())
}

/// The named array field of a response, or a protocol error.
fn array<'a>(doc: &'a Value, key: &str, what: &str) -> Result<&'a Vec<Value>, GoogleError> {
    doc.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| GoogleError::protocol(format!("{what} response had no {key} array")))
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
