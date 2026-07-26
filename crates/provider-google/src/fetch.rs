//! Label-list, message snapshot/history-delta fetch + paging, and raw-source fetch for
//! the Gmail provider.
//!
//! Gmail's sync is **account-global** (`historyId`), not per-folder like Graph, so there
//! are two shapes rather than one unified delta:
//!
//! - **Snapshot** ([`snapshot_page`], `cursor` `None`): `users.messages.list` enumerates `{id,
//!   threadId}` refs (paginated by `pageToken`, optionally windowed by a `q: after:<epoch>` floor);
//!   each id is fetched full (`messages.get?format=metadata`) and normalized. The cursor to persist
//!   is the account `historyId` captured *before* the enumeration (any messages that arrive
//!   mid-snapshot are simply re-reported by the first delta — idempotent).
//! - **Delta** ([`delta_page`], `cursor` `Some`): `users.history.list?startHistoryId=…` returns
//!   `messagesAdded`/`labelsAdded`/`labelsRemoved` (whose message objects are *partials* — id +
//!   labelIds only) and `messagesDeleted`. Every touched-but-present id is **re-fetched** full (the
//!   engine applies whole objects); deleted ids tombstone. A `404` (the `startHistoryId` aged out
//!   of the window) becomes [`GoogleError::HistoryExpired`] → the stream restarts as a snapshot.

use engine_core::{
    ids::ProviderKey,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::SyncState,
    time::CalendarDate,
};
use engine_provider::{PageToken, SyncKind, SyncPage};
use serde_json::Value;

use crate::{
    base64url,
    error::GoogleError,
    json::{opt_str, req_str},
    normalize::{METADATA_HEADERS, all_mail_mailbox, label_from_json, message_from_json},
    transport::{GoogleClient, encode_query_value},
};

/// The Gmail user-scoped API root (`/gmail/v1/users/me`).
const USERS_ME: &str = "/gmail/v1/users/me";

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

/// Fetches one snapshot page: a `messages.list` page whose ids are each fetched full.
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

    let mut changed = Vec::new();
    let mut present = Vec::new();
    // `messages` is absent on an empty mailbox / final empty page — treat as no rows.
    let entries = doc.get("messages").and_then(Value::as_array);
    for entry in entries.into_iter().flatten() {
        let id = req_str(entry, "id")?;
        match get_message(client, id).await {
            Ok(message) => {
                present.push(message.id.key().clone());
                changed.push(message);
            }
            // Deleted in the race between list and get → skip; a later delta reports it.
            Err(GoogleError::Status { status: 404, .. }) => {}
            Err(other) => return Err(other),
        }
    }

    Ok(SyncPage {
        kind: SyncKind::Snapshot,
        changed,
        removed: Vec::new(),
        present,
        next_page: opt_str(&doc, "nextPageToken").map(PageToken::new),
        next_cursor: history_id.clone(),
        total: None,
    })
}

/// Fetches one history-delta page from `cursor` (a `historyId`): re-fetches each changed
/// message and tombstones each deleted one. A `404` (aged-out cursor) becomes
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

    let (changed_ids, removed) = collect_history(&doc)?;
    let mut changed = Vec::new();
    for id in changed_ids {
        match get_message(client, &id).await {
            Ok(message) => changed.push(message),
            // Changed then deleted in the same window → skip (a tombstone covers it).
            Err(GoogleError::Status { status: 404, .. }) => {}
            Err(other) => return Err(other),
        }
    }

    // Gmail returns the latest historyId even when nothing changed, so the cursor always
    // advances; fall back to the prior cursor only if the field is somehow absent.
    let next_cursor = opt_str(&doc, "historyId").map_or_else(|| cursor.clone(), SyncState::new);
    Ok(SyncPage {
        kind: SyncKind::Delta,
        changed,
        removed,
        present: Vec::new(),
        next_page: opt_str(&doc, "nextPageToken").map(PageToken::new),
        next_cursor,
        total: None,
    })
}

/// Collects a history page's changed message ids (present, to re-fetch as full objects)
/// and removed keys (tombstones). A message added-then-deleted in the same window is
/// only tombstoned. The changed ids stay as strings — they address the re-fetch `get`.
fn collect_history(doc: &Value) -> Result<(Vec<String>, Vec<ProviderKey>), GoogleError> {
    let mut removed_keys = Vec::new();
    let mut removed_ids = std::collections::BTreeSet::new();
    let mut changed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    let records = doc.get("history").and_then(Value::as_array);
    // First pass: deletions win, so gather them before deciding what to re-fetch.
    for record in records.into_iter().flatten() {
        for id in message_ids(record, "messagesDeleted") {
            if removed_ids.insert(id.clone()) {
                removed_keys.push(
                    ProviderKey::new(&id)
                        .map_err(|e| GoogleError::protocol(format!("bad deleted id: {e}")))?,
                );
            }
        }
    }
    for record in records.into_iter().flatten() {
        for group in ["messagesAdded", "labelsAdded", "labelsRemoved"] {
            for id in message_ids(record, group) {
                if !removed_ids.contains(&id) && seen.insert(id.clone()) {
                    changed.push(id);
                }
            }
        }
    }
    Ok((changed, removed_keys))
}

/// The `message.id`s inside a history record's `group` array (e.g. `messagesAdded`).
fn message_ids(record: &Value, group: &str) -> Vec<String> {
    record
        .get(group)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("message").and_then(|m| opt_str(m, "id")))
        .map(str::to_owned)
        .collect()
}

/// The `messages.list` URL: a continuation `pageToken`, else the first page, optionally
/// windowed by an `after:<epoch>` floor.
fn list_url(
    client: &GoogleClient,
    page: Option<&PageToken>,
    floor: Option<CalendarDate>,
) -> String {
    use std::fmt::Write as _;
    let mut url = format!("{USERS_ME}/messages?maxResults=100");
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
