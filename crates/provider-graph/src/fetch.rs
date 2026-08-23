//! Folder-list and message snapshot/delta fetch + paging for the Graph provider.
//!
//! Two passes feed [`messages_page`]:
//! - **snapshot** (`cursor` `None`): the initial `messages/delta` enumeration returns *full*
//!   objects; each becomes a `changed` + `present` entry, and the pass ends at the
//!   `@odata.deltaLink` (the cursor to persist).
//! - **incremental delta** (`cursor` `Some`): a changed entry is normally a *full* object (it
//!   carries `@odata.etag`) and is used directly; a *lightweight* change (e.g. `isRead`) returns an
//!   etag-less *partial*, which is resolved as a **state change** through the narrow
//!   [`MESSAGE_STATE_SELECT`] rather than re-fetched whole. `@removed` tombstones apply inline.
//!   Multi-page passes follow `@odata.nextLink`.

use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{MailState, MailStateChange, Mailbox, Message},
    raw::RawMime,
    sync::SyncState,
    time::CalendarDate,
};
use engine_provider::{PageToken, SyncKind, SyncPage};
use serde_json::Value;

use crate::{
    error::GraphError,
    json::{req_str, wrap_id},
    normalize::{
        MESSAGE_EXPAND, MESSAGE_SELECT, WELL_KNOWN_ROLES, apply_roles, folder_from_json,
        message_from_json, well_known_folder_id,
    },
    normalize_state::{MESSAGE_STATE_SELECT, state_from_json},
    transport::GraphClient,
};

/// Cursor placeholder for an intermediate page (the orchestrator ignores
/// `next_cursor` until the final page carries the `@odata.deltaLink`).
const PENDING_CURSOR: &str = "graph-pending";

/// Fetches the account's mail folders as a snapshot, with roles resolved from the
/// well-known aliases (display names are localized, so a role can't be read off
/// them).
pub(crate) async fn folders(client: &GraphClient) -> Result<Vec<Mailbox>, GraphError> {
    let root = well_known_id(client, "msgfolderroot").await?;
    let mut resolved = Vec::with_capacity(WELL_KNOWN_ROLES.len());
    for (alias, role) in WELL_KNOWN_ROLES {
        // A well-known folder the account never provisioned 404s; skip its role
        // rather than failing the whole folder list.
        if let Some(id) = optional_well_known_id(client, alias).await? {
            resolved.push((id, role.clone()));
        }
    }
    // Drain every page of the folder list (`@odata.nextLink`), so a mailbox with
    // more than one page of folders is not truncated — and then tombstoned, since
    // this set becomes the snapshot's `present` set.
    let mut mailboxes = Vec::new();
    let mut url = client.url("/mailFolders?$top=100");
    loop {
        let doc = client.get(&url).await?;
        for folder in value_array(&doc, "mailFolders")? {
            mailboxes.push(folder_from_json(folder, Some(&root))?);
        }
        match odata_link(&doc, "@odata.nextLink") {
            Some(next) => url = next,
            None => break,
        }
    }
    apply_roles(&mut mailboxes, &resolved);
    Ok(mailboxes)
}

/// Resolves a well-known folder alias (`inbox`, `msgfolderroot`, …) to its id.
async fn well_known_id(client: &GraphClient, alias: &str) -> Result<MailboxId, GraphError> {
    let doc = client
        .get(&client.url(&format!("/mailFolders/{alias}?$select=id")))
        .await?;
    well_known_folder_id(&doc)
}

/// Resolves a well-known alias to its folder id, returning `None` when the account
/// has no such folder (`404`) and propagating any other failure.
async fn optional_well_known_id(
    client: &GraphClient,
    alias: &str,
) -> Result<Option<MailboxId>, GraphError> {
    match well_known_id(client, alias).await {
        Ok(id) => Ok(Some(id)),
        Err(GraphError::Status { status: 404, .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Re-fetches one full message by id (the delta changed-id re-fetch).
pub(crate) async fn message(client: &GraphClient, id: &MessageId) -> Result<Message, GraphError> {
    let select = MESSAGE_SELECT.join(",");
    let doc = client
        .get(&client.url(&format!(
            "/messages/{}?$select={select}&$expand={MESSAGE_EXPAND}",
            id.as_str()
        )))
        .await?;
    message_from_json(&doc)
}

/// Fetches one message's raw RFC 822 MIME via the `$value` endpoint — the source the
/// reading view renders. Graph accepts the message id verbatim in the path, exactly as
/// the changed-id re-fetch in [`message`] does.
pub(crate) async fn message_source(
    client: &GraphClient,
    key: &ProviderKey,
) -> Result<RawMime, GraphError> {
    let bytes = client
        .get_bytes(&client.url(&format!("/messages/{}/$value", key.as_str())))
        .await?;
    Ok(RawMime::new(bytes))
}

/// Reads one message's **state** through the narrow `$select` — what an etag-less delta
/// entry costs to resolve, in place of [`message`]'s whole object.
async fn message_state(client: &GraphClient, key: &ProviderKey) -> Result<MailState, GraphError> {
    let select = MESSAGE_STATE_SELECT.join(",");
    let doc = client
        .get(&client.url(&format!("/messages/{}?$select={select}", key.as_str())))
        .await?;
    state_from_json(&doc)
}

/// Fetches one page of the bound folder's messages (see the module docs). `floor`
/// (the per-sync window's date floor) windows the **initial** snapshot to messages
/// received on or after that date; later pages follow the server's links, which
/// carry the window.
pub(crate) async fn messages_page(
    client: &GraphClient,
    folder: &MailboxId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    floor: Option<CalendarDate>,
) -> Result<SyncPage<Message>, GraphError> {
    let kind = if cursor.is_none() {
        SyncKind::Snapshot
    } else {
        SyncKind::Delta
    };
    let doc = client
        .get(&page_url(client, folder, cursor, page, floor))
        .await?;

    let mut changed = Vec::new();
    let mut patched = Vec::new();
    let mut removed = Vec::new();
    let mut present = Vec::new();
    for entry in value_array(&doc, "messages delta")? {
        if entry.get("@removed").is_some() {
            removed.push(entry_key(entry)?);
            continue;
        }
        // Per the delta-query-messages docs a changed entry is a FULL object — and it
        // is for most edits; a full message resource carries `@odata.etag`. The
        // exception is a *lightweight* property change (notably `isRead` on consumer
        // mailboxes), which returns only the changed property + id and no etag.
        //
        // In a delta that is a state change, and resolving it costs the narrow
        // `$select` rather than the whole message. A **snapshot** never takes this
        // branch: its entries are always full, and a state change carries no key into
        // `present`, so one here would be tombstoned at the end of the pass.
        if kind == SyncKind::Delta && entry.get("@odata.etag").is_none() {
            let key = entry_key(entry)?;
            match message_state(client, &key).await {
                Ok(state) => patched.push(MailStateChange::new(key, state)),
                // Deleted/moved in the race since the delta → skip; a later delta
                // reports the removal, so the pass is not wedged.
                Err(GraphError::Status { status: 404, .. }) => {}
                Err(other) => return Err(other),
            }
            continue;
        }
        let full = if entry.get("@odata.etag").is_some() {
            message_from_json(entry)?
        } else {
            let id = MessageId::new(entry_key(entry)?);
            match message(client, &id).await {
                Ok(full) => full,
                Err(GraphError::Status { status: 404, .. }) => continue,
                Err(other) => return Err(other),
            }
        };
        if kind == SyncKind::Snapshot {
            present.push(full.id.key().clone());
        }
        changed.push(full);
    }

    let next_page = odata_link(&doc, "@odata.nextLink").map(PageToken::new);
    let next_cursor = match odata_link(&doc, "@odata.deltaLink") {
        Some(delta) => SyncState::new(delta),
        None => cursor
            .cloned()
            .unwrap_or_else(|| SyncState::new(PENDING_CURSOR)),
    };
    Ok(SyncPage {
        kind,
        changed,
        patched,
        removed,
        present,
        next_page,
        next_cursor,
        total: None,
    })
}

/// The URL for the next page: a continuation `@odata.nextLink`, else the delta
/// `cursor` (an `@odata.deltaLink`), else the folder's first `messages/delta` call —
/// which, when `floor` is set, carries a `receivedDateTime` window.
fn page_url(
    client: &GraphClient,
    folder: &MailboxId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    floor: Option<CalendarDate>,
) -> String {
    if let Some(page) = page {
        page.as_str().to_owned()
    } else if let Some(cursor) = cursor {
        cursor.as_str().to_owned()
    } else {
        let select = MESSAGE_SELECT.join(",");
        let mut path = format!(
            "/mailFolders/{}/messages/delta?$select={select}&$expand={MESSAGE_EXPAND}",
            folder.as_str()
        );
        if let Some(date) = floor {
            // Message delta accepts a `$filter` on `receivedDateTime` on the **initial**
            // request; the returned deltaLink carries the window, so later pages must not
            // re-specify it. The space around `ge` is percent-encoded for a valid query.
            path.push_str(&since_filter(date));
        }
        client.url(&path)
    }
}

/// The `receivedDateTime` lower-bound filter clause for the initial delta, windowing the
/// sync to `date` 00:00:00 UTC (`&$filter=receivedDateTime ge YYYY-MM-DDT00:00:00Z`, with
/// the operator spaces percent-encoded).
fn since_filter(date: CalendarDate) -> String {
    format!("&$filter=receivedDateTime%20ge%20{date}T00:00:00Z")
}

/// The `value` array of a Graph collection response, or a protocol error.
fn value_array<'a>(doc: &'a Value, what: &str) -> Result<&'a Vec<Value>, GraphError> {
    doc.get("value")
        .and_then(Value::as_array)
        .ok_or_else(|| GraphError::protocol(format!("{what} response had no value array")))
}

/// The `ProviderKey` of a delta entry (its `id`).
fn entry_key(entry: &Value) -> Result<ProviderKey, GraphError> {
    wrap_id(ProviderKey::new(req_str(entry, "id")?), "message id")
}

/// An `@odata.*` link field as an owned absolute URL.
fn odata_link(doc: &Value, key: &str) -> Option<String> {
    doc.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
