//! Applying a [`MailboxEdit`] to Gmail's labels.
//!
//! Gmail has no folder tree. A label nests under another only because its **name** starts
//! with the other's name and a `/` (`normalize::nest_labels`), and its id never moves. So
//! every edit here is reasoned over the account's raw label list, by full name:
//!
//! - a create is `labels.create` under `<parent's full name>/<name>`;
//! - a rename or move is a `labels.patch` of the name, **and of every label beneath it**, since
//!   nothing else carries the nesting. The labels beneath go first and the folder itself last, so
//!   an attempt cut short leaves the folder under its old name and a retry finishes the job;
//! - a trash cannot file a label under Trash, which is a system label: each message the subtree
//!   holds that is filed nowhere else goes to Trash (one `messages.batchModify` per thousand), and
//!   the labels are then removed with `labels.delete`, which unlinks them from every message;
//! - a delete is `labels.delete` of the folder and every label beneath it. Gmail keeps the mail in
//!   All Mail, so nothing is destroyed.
//!
//! A system label (`INBOX`, `TRASH`, …) and the synthetic All Mail are not the user's to change,
//! and refusing them here means nothing is sent that Gmail would refuse anyway.

use std::collections::{BTreeSet, HashSet};

use engine_core::ids::MailboxId;
use engine_provider::{MailboxEdit, MailboxEditReceipt, ProviderError, ProviderResult};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};

use crate::{
    error::GoogleError, fetch::MAX_CONCURRENT_GETS, json::req_str, normalize::ALL_MAIL_ID,
    transport::GoogleClient,
};

const LABELS: &str = "/gmail/v1/users/me/labels";
const MESSAGES: &str = "/gmail/v1/users/me/messages";

/// The most message ids one `messages.batchModify` takes.
const BATCH_LIMIT: usize = 1000;

/// A page of `messages.list`, at its documented maximum.
const LIST_PAGE: usize = 500;

/// The system labels that file a message somewhere the user will look for it. A message
/// carrying one of these (or a user label outside the trashed subtree) is not trashed with the
/// subtree: it only loses the subtree's labels. `IMPORTANT`, the categories and the state labels
/// are not places, so a message carrying only those is filed nowhere else.
const PLACEMENTS: &[&str] = &["INBOX", "SENT", "DRAFT"];

/// One label as `users.labels.list` reports it: the full `/`-joined name, not the own name
/// the folder list shows.
#[derive(Debug, Clone)]
struct Label {
    id: String,
    name: String,
    user: bool,
}

/// Applies `edit` to the account's labels.
///
/// # Errors
///
/// A classified [`ProviderError`]: a label that is gone, or a name another label already has,
/// is a [`Conflict`](engine_core::error::FailureClass::Conflict); a system label is
/// [`InvalidState`](engine_core::error::FailureClass::InvalidState); the rest map from the
/// HTTP status.
pub(crate) async fn edit(
    client: &GoogleClient,
    edit: &MailboxEdit,
) -> ProviderResult<MailboxEditReceipt> {
    let all = labels(client).await?;
    match edit {
        MailboxEdit::Create { name, parent } => create(client, &all, name, parent.as_ref()).await,
        MailboxEdit::Update {
            target,
            name,
            parent,
        } => update(client, &all, target, name, parent.as_ref()).await,
        MailboxEdit::Trash { target, trash, .. } => {
            trash_subtree(client, &all, target, trash).await
        }
        MailboxEdit::Delete { target } => delete(client, &all, target).await,
    }
}

async fn create(
    client: &GoogleClient,
    all: &[Label],
    name: &str,
    parent: Option<&MailboxId>,
) -> ProviderResult<MailboxEditReceipt> {
    let full = joined(&prefix(all, parent)?, name);
    // A retry after the first attempt landed meets the label it made.
    if let Some(existing) = all.iter().find(|label| label.name == full) {
        return resolved(&existing.id);
    }
    let body = json!({
        "name": full,
        "labelListVisibility": "labelShow",
        "messageListVisibility": "show",
    });
    let created = client
        .post(&client.url(LABELS), "application/json", encode(&body)?)
        .await?
        .ok_or_else(|| GoogleError::protocol("labels.create returned no label"))?;
    resolved(req_str(&created, "id")?)
}

async fn update(
    client: &GoogleClient,
    all: &[Label],
    target: &MailboxId,
    name: &str,
    parent: Option<&MailboxId>,
) -> ProviderResult<MailboxEditReceipt> {
    let label = user_label(all, target)?;
    let full = joined(&prefix(all, parent)?, name);
    if full == label.name {
        return resolved(&label.id);
    }
    let old_prefix = format!("{}/", label.name);
    let renames: Vec<(&Label, String)> = all
        .iter()
        .filter_map(|other| {
            let rest = other.name.strip_prefix(&old_prefix)?;
            Some((other, format!("{full}/{rest}")))
        })
        .collect();
    // Checked before anything is written: a clash found halfway would leave the tree split
    // between two names.
    let moving: HashSet<&str> = renames
        .iter()
        .map(|(other, _)| other.id.as_str())
        .chain([label.id.as_str()])
        .collect();
    let wanted = renames
        .iter()
        .map(|(_, new)| new.as_str())
        .chain([full.as_str()]);
    for new in wanted {
        if all
            .iter()
            .any(|other| !moving.contains(other.id.as_str()) && other.name == new)
        {
            return Err(ProviderError::conflict(
                "another label already has that name",
            ));
        }
    }
    for (other, new) in &renames {
        rename(client, &other.id, new).await?;
    }
    rename(client, &label.id, &full).await?;
    resolved(&label.id)
}

async fn trash_subtree(
    client: &GoogleClient,
    all: &[Label],
    target: &MailboxId,
    trash: &MailboxId,
) -> ProviderResult<MailboxEditReceipt> {
    let label = user_label(all, target)?;
    let subtree = subtree(all, label);
    let inside: HashSet<&str> = subtree.iter().map(|l| l.id.as_str()).collect();
    let user: HashSet<&str> = all
        .iter()
        .filter(|l| l.user)
        .map(|l| l.id.as_str())
        .collect();

    // `messages.list` leaves out what is already in Trash, so a retry picks up where an
    // interrupted attempt stopped.
    let mut held = BTreeSet::new();
    for label in &subtree {
        held.extend(messages_with(client, &label.id).await?);
    }
    let (user, inside) = (&user, &inside);
    let to_trash: Vec<String> = stream::iter(held.into_iter().map(|id| async move {
        let Some(labels) = message_labels(client, &id).await? else {
            return Ok::<_, GoogleError>(None);
        };
        let elsewhere = labels.iter().any(|l| {
            PLACEMENTS.contains(&l.as_str())
                || (user.contains(l.as_str()) && !inside.contains(l.as_str()))
        });
        Ok((!elsewhere).then_some(id))
    }))
    .buffer_unordered(MAX_CONCURRENT_GETS)
    .try_filter_map(|id| async move { Ok(id) })
    .try_collect()
    .await?;

    for chunk in to_trash.chunks(BATCH_LIMIT) {
        let body = json!({ "ids": chunk, "addLabelIds": [trash.as_str()], "removeLabelIds": [] });
        client
            .post(
                &client.url(&format!("{MESSAGES}/batchModify")),
                "application/json",
                encode(&body)?,
            )
            .await?;
    }
    remove_labels(client, &subtree).await?;
    Ok(MailboxEditReceipt::removed())
}

async fn delete(
    client: &GoogleClient,
    all: &[Label],
    target: &MailboxId,
) -> ProviderResult<MailboxEditReceipt> {
    // Already gone is done: the op behind this is retried.
    let Some(label) = all.iter().find(|l| l.id == target.as_str()) else {
        return Ok(MailboxEditReceipt::removed());
    };
    if !label.user {
        return Err(system_label());
    }
    remove_labels(client, &subtree(all, label)).await?;
    Ok(MailboxEditReceipt::removed())
}

/// The account's labels, full names intact.
async fn labels(client: &GoogleClient) -> Result<Vec<Label>, GoogleError> {
    let doc = client.get(&client.url(LABELS)).await?;
    doc.get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|label| {
            Ok(Label {
                id: req_str(label, "id")?.to_owned(),
                name: req_str(label, "name")?.to_owned(),
                user: label.get("type").and_then(Value::as_str) == Some("user"),
            })
        })
        .collect()
}

/// `label` and every label whose name puts it beneath, deepest first so a parent never goes
/// before what it holds.
fn subtree<'a>(all: &'a [Label], label: &'a Label) -> Vec<&'a Label> {
    let prefix = format!("{}/", label.name);
    let mut labels: Vec<&Label> = all
        .iter()
        .filter(|other| other.name.starts_with(&prefix))
        .chain([label])
        .collect();
    labels.sort_by_key(|l| core::cmp::Reverse(l.name.matches('/').count()));
    labels
}

/// A user label the edit may change: gone is a conflict, a system label (and the All Mail this
/// adapter synthesizes, which Gmail has no label for) is refused.
fn user_label<'a>(all: &'a [Label], id: &MailboxId) -> ProviderResult<&'a Label> {
    if id.as_str() == ALL_MAIL_ID {
        return Err(system_label());
    }
    let label = all
        .iter()
        .find(|l| l.id == id.as_str())
        .ok_or_else(|| ProviderError::conflict("the label is gone"))?;
    if label.user {
        Ok(label)
    } else {
        Err(system_label())
    }
}

/// The full name a label made inside `parent` starts with: empty at the top level.
fn prefix(all: &[Label], parent: Option<&MailboxId>) -> ProviderResult<String> {
    match parent {
        None => Ok(String::new()),
        Some(parent) => Ok(user_label(all, parent)?.name.clone()),
    }
}

fn joined(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn system_label() -> ProviderError {
    ProviderError::invalid_state("a system label cannot be changed")
}

/// `labels.patch` of the name; a label that has gone meanwhile is a conflict.
async fn rename(client: &GoogleClient, id: &str, name: &str) -> ProviderResult<()> {
    let body = encode(&json!({ "name": name }))?;
    match client
        .patch(
            &client.url(&format!("{LABELS}/{id}")),
            "application/json",
            None,
            body,
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(err) if is_not_found(&err) => Err(ProviderError::conflict("the label is gone")),
        Err(err) => Err(err.into()),
    }
}

/// `labels.delete` of each label, a label already gone counting as removed.
async fn remove_labels(client: &GoogleClient, labels: &[&Label]) -> ProviderResult<()> {
    for label in labels {
        match client
            .delete(&client.url(&format!("{LABELS}/{}", label.id)), None)
            .await
        {
            Ok(()) => {}
            Err(err) if is_not_found(&err) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// The ids of every message carrying `label`, outside Spam and Trash, across all pages.
async fn messages_with(client: &GoogleClient, label: &str) -> Result<Vec<String>, GoogleError> {
    let mut ids = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let mut url = client.url(&format!(
            "{MESSAGES}?labelIds={label}&maxResults={LIST_PAGE}"
        ));
        if let Some(token) = &page {
            url.push_str("&pageToken=");
            url.push_str(token);
        }
        let doc = client.get(&url).await?;
        for message in doc
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            ids.push(req_str(message, "id")?.to_owned());
        }
        match doc.get("nextPageToken").and_then(Value::as_str) {
            Some(token) => page = Some(token.to_owned()),
            None => return Ok(ids),
        }
    }
}

/// A message's current `labelIds`, or `None` when it has gone since it was listed.
async fn message_labels(
    client: &GoogleClient,
    id: &str,
) -> Result<Option<Vec<String>>, GoogleError> {
    let key = engine_core::ids::ProviderKey::new(id)
        .map_err(|_| GoogleError::protocol("messages.list returned an empty id"))?;
    match crate::fetch::message_labels(client, &key).await {
        Ok(labels) => Ok(Some(labels)),
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_not_found(err: &GoogleError) -> bool {
    matches!(err, GoogleError::Status { status: 404, .. })
}

fn resolved(id: &str) -> ProviderResult<MailboxEditReceipt> {
    let id = MailboxId::try_from(id)
        .map_err(|_| ProviderError::from(GoogleError::protocol("a label with an empty id")))?;
    Ok(MailboxEditReceipt::resolved(id))
}

fn encode(body: &Value) -> Result<Vec<u8>, GoogleError> {
    serde_json::to_vec(body).map_err(GoogleError::from)
}

#[cfg(test)]
#[path = "labels_write_tests.rs"]
mod tests;
