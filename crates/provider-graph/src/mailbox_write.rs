//! Changing the account's folder tree: [`MailboxEdit`] onto Graph's `mailFolder` verbs.
//!
//! | edit | request |
//! |---|---|
//! | create | `POST /mailFolders/{parent}/childFolders {displayName}`; the top level is the well-known `msgfolderroot` |
//! | rename | `PATCH /mailFolders/{id} {displayName}` |
//! | move | `POST /mailFolders/{id}/move {destinationId}` |
//! | trash | a rename to the name chosen for Trash, then a move into it |
//! | delete | `POST /mailFolders/{id}/permanentDelete` |
//!
//! A folder's id is constant across a rename and a move: containers take no part in
//! immutable ids, "but their regular IDs were already constant"
//! (<https://learn.microsoft.com/en-us/graph/outlook-immutable-id>). The receipt still
//! names the id the move answered with rather than assuming it.
//!
//! **An update reads the folder first**, because [`MailboxEdit::Update`] states where the
//! folder is to end up rather than what changed: only the half that differs is sent, so a
//! rename is never also a move to where the folder already is. The rename goes first, so a
//! trashed folder arrives in Trash under the name chosen to be free there rather than
//! under the one that may already be taken.
//!
//! **Delete is `permanentDelete`, not `DELETE`.** The docs have `DELETE` remove the folder
//! without saying where to, and Outlook files a deleted folder in Deleted Items, which is
//! what [`MailboxEdit::Trash`] already does explicitly; `permanentDelete` "removes its items
//! from the user's mailbox" and does not place the folder in Purges
//! (<https://learn.microsoft.com/en-us/graph/api/mailfolder-permanentdelete>), which is the
//! permanent removal the neutral verb promises. It is served on the global cloud only; the
//! national clouds lack it, as they lack the message `permanentDelete` drafts use.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::{MailboxEdit, MailboxEditReceipt, ProviderError, ProviderResult};
use serde_json::{Value, json};

use crate::{
    error::GraphError,
    fetch::well_known_id,
    json::{opt_str, req_str, wrap_id},
    transport::GraphClient,
};

/// The well-known name of the folder that holds the top level of the tree.
const ROOT: &str = "msgfolderroot";

/// Applies one folder edit.
///
/// # Errors
///
/// A classified [`ProviderError`]: a folder that is not there, or a name already taken
/// beside it, is [`FailureClass::Conflict`]; everything else is the status classification.
pub(crate) async fn edit_mailbox(
    client: &GraphClient,
    edit: &MailboxEdit,
) -> ProviderResult<MailboxEditReceipt> {
    match edit {
        MailboxEdit::Create { name, parent } => create(client, name, parent.as_ref())
            .await
            .map(MailboxEditReceipt::resolved),
        MailboxEdit::Update {
            target,
            name,
            parent,
        } => {
            let destination = match parent {
                Some(parent) => parent.clone(),
                None => well_known_id(client, ROOT).await?,
            };
            relocate(client, target, name, &destination)
                .await
                .map(MailboxEditReceipt::resolved)
        }
        MailboxEdit::Trash {
            target,
            trash,
            name,
        } => relocate(client, target, name, trash)
            .await
            .map(MailboxEditReceipt::resolved),
        MailboxEdit::Delete { target } => {
            delete(client, target).await?;
            Ok(MailboxEditReceipt::removed())
        }
    }
}

/// Creates `name` inside `parent`, or finds the folder of that name a previous attempt
/// already made.
async fn create(
    client: &GraphClient,
    name: &str,
    parent: Option<&MailboxId>,
) -> ProviderResult<MailboxId> {
    let parent = parent.map_or(ROOT, MailboxId::as_str);
    let body = serde_json::to_vec(&json!({ "displayName": name })).map_err(GraphError::from)?;
    match client
        .post(
            &client.url(&format!("/mailFolders/{parent}/childFolders")),
            "application/json",
            body,
        )
        .await
    {
        Ok(created) => folder_id(created.as_ref()),
        Err(err) if name_taken(&err) => match child_named(client, parent, name).await? {
            // The op behind this is retried, and a retry meets the folder the first
            // attempt made: that is the create having succeeded.
            Some(existing) => Ok(existing),
            // Taken by a name differing only in case, which is not the folder asked for.
            None => Err(conflict(err, "a folder of that name already exists there")),
        },
        Err(err) => Err(err.into()),
    }
}

/// Gives `target` the name `name` and the parent `destination`, sending only what differs.
async fn relocate(
    client: &GraphClient,
    target: &MailboxId,
    name: &str,
    destination: &MailboxId,
) -> ProviderResult<MailboxId> {
    let path = format!("/mailFolders/{}", target.as_str());
    let current = match client
        .get(&client.url(&format!("{path}?$select=id,displayName,parentFolderId")))
        .await
    {
        Ok(doc) => doc,
        Err(err) if is_not_found(&err) => return Err(conflict(err, "the folder is gone")),
        Err(err) => return Err(err.into()),
    };

    if req_str(&current, "displayName")? != name {
        let body = serde_json::to_vec(&json!({ "displayName": name })).map_err(GraphError::from)?;
        client
            .patch(&client.url(&path), "application/json", None, body)
            .await
            .map_err(|err| refusal(err, "the destination already holds a folder of that name"))?;
    }

    if opt_str(&current, "parentFolderId") == Some(destination.as_str()) {
        return Ok(target.clone());
    }
    let body = serde_json::to_vec(&json!({ "destinationId": destination.as_str() }))
        .map_err(GraphError::from)?;
    let moved = client
        .post(
            &client.url(&format!("{path}/move")),
            "application/json",
            body,
        )
        .await
        .map_err(|err| refusal(err, "the folder could not be moved there"))?;
    match moved {
        Some(doc) => folder_id(Some(&doc)),
        None => Ok(target.clone()),
    }
}

/// Removes `target` for good; one already gone is the state asked for.
async fn delete(client: &GraphClient, target: &MailboxId) -> Result<(), GraphError> {
    match client
        .post(
            &client.url(&format!("/mailFolders/{}/permanentDelete", target.as_str())),
            "application/json",
            Vec::new(),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(err) if is_not_found(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// The folder called exactly `name` directly inside `parent`, reading every page.
async fn child_named(
    client: &GraphClient,
    parent: &str,
    name: &str,
) -> Result<Option<MailboxId>, GraphError> {
    let mut url = client.url(&format!(
        "/mailFolders/{parent}/childFolders?$top=100&$select=id,displayName"
    ));
    loop {
        let doc = client.get(&url).await?;
        let folders = doc
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(|| GraphError::protocol("childFolders response had no value array"))?;
        if let Some(found) = folders
            .iter()
            .find(|f| opt_str(f, "displayName") == Some(name))
        {
            return folder_id(Some(found))
                .map(Some)
                .map_err(|_| GraphError::protocol("a listed child folder carried no id"));
        }
        match opt_str(&doc, "@odata.nextLink") {
            Some(next) => next.clone_into(&mut url),
            None => return Ok(None),
        }
    }
}

/// The `id` of a folder object Graph answered with.
fn folder_id(doc: Option<&Value>) -> ProviderResult<MailboxId> {
    let doc = doc.ok_or_else(|| GraphError::protocol("folder write returned no folder"))?;
    Ok(wrap_id(
        MailboxId::try_from(req_str(doc, "id")?),
        "mail folder id",
    )?)
}

/// Whether Graph refused because a folder of that name is already there.
///
/// Matched on the documented-by-convention code as well as the status, since a duplicate
/// that came back as a `400` would otherwise read as a permanent failure. Not observed
/// against a live mailbox yet.
fn name_taken(err: &GraphError) -> bool {
    matches!(err, GraphError::Status { status: 409, .. })
        || matches!(err, GraphError::Status { code: Some(code), .. } if code == "ErrorFolderExists")
}

/// Whether Graph answered that the folder is not there.
fn is_not_found(err: &GraphError) -> bool {
    matches!(err, GraphError::Status { status: 404, .. })
        || matches!(err, GraphError::Status { code: Some(code), .. } if code == "ErrorItemNotFound")
}

/// A write that met a folder gone or a name taken is a conflict; anything else keeps the
/// status classification.
fn refusal(err: GraphError, taken: &str) -> ProviderError {
    if is_not_found(&err) {
        conflict(err, "the folder is gone")
    } else if name_taken(&err) {
        conflict(err, taken)
    } else {
        err.into()
    }
}

/// A [`FailureClass::Conflict`] carrying `err` as its source.
fn conflict(err: GraphError, detail: &str) -> ProviderError {
    ProviderError::new(FailureClass::Conflict, detail).with_source(err)
}

#[cfg(test)]
#[path = "mailbox_write_tests.rs"]
mod tests;
