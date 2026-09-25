//! Changing the account's folder tree via `Mailbox/set` (RFC 8621 §2.5).
//!
//! A JMAP mailbox id is a server id that survives a rename and a move, so every edit but a
//! create resolves to the id it named:
//!
//! - [`MailboxEdit::Create`] → one `create` with `isSubscribed: true`, so the folder shows in
//!   clients that list subscriptions only.
//! - [`MailboxEdit::Update`] and [`MailboxEdit::Trash`] → one `update` setting `name` and
//!   `parentId`; trashing is a move under Trash, which carries the folder's mail and subfolders
//!   with it.
//! - [`MailboxEdit::Delete`] → a `destroy` of the whole subtree with `onDestroyRemoveEmails: true`.
//!   RFC 8621 refuses to destroy a mailbox that still has children (`mailboxHasChild`), so the list
//!   is ordered deepest first; Stalwart applies a `destroy` list in order (measured).
//!
//! Stalwart's answers, measured against the harness: a sibling of the same name is
//! `alreadyExists` carrying the sibling's `existingId`, for a create and an update alike; a
//! missing or cyclic `parentId` is `invalidProperties` (the cycle names `parentId`, the
//! missing parent names nothing); a missing target is `notFound`.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use engine_core::ids::{AccountId, MailboxId};
use engine_provider::{
    MailboxEdit, MailboxEditReceipt, MailboxWrites, ProviderError, ProviderResult,
};
use serde_json::{Map, Value, json};

use crate::{
    JmapProvider,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// The creation id of the one mailbox a create sends.
const NEW: &str = "new";

#[async_trait]
impl MailboxWrites for JmapProvider {
    async fn edit_mailbox(
        &self,
        _account: &AccountId,
        edit: &MailboxEdit,
    ) -> ProviderResult<MailboxEditReceipt> {
        let account = self.mail_account()?;
        edit_mailbox(self.executor.as_ref(), &account, edit).await
    }
}

/// Applies `edit` to the tree of `mail_account`.
///
/// # Errors
///
/// Returns a classified [`ProviderError`]: a target that is gone, a destination that is
/// gone or inside the folder, and a name already taken beside it are conflicts; a name the
/// server refuses is invalid state; anything else keeps the class its `SetError` or
/// transport failure maps to.
pub(crate) async fn edit_mailbox(
    executor: &dyn Executor,
    mail_account: &str,
    edit: &MailboxEdit,
) -> ProviderResult<MailboxEditReceipt> {
    match edit {
        MailboxEdit::Create { name, parent } => {
            create(executor, mail_account, name, parent.as_ref()).await
        }
        MailboxEdit::Update {
            target,
            name,
            parent,
        } => place(executor, mail_account, target, name, parent.as_ref()).await,
        MailboxEdit::Trash {
            target,
            trash,
            name,
        } => place(executor, mail_account, target, name, Some(trash)).await,
        MailboxEdit::Delete { target } => destroy_subtree(executor, mail_account, target).await,
    }
}

async fn create(
    executor: &dyn Executor,
    mail_account: &str,
    name: &str,
    parent: Option<&MailboxId>,
) -> ProviderResult<MailboxEditReceipt> {
    let mut create = Map::new();
    create.insert(
        NEW.to_owned(),
        json!({ "name": name, "parentId": parent.map(MailboxId::as_str), "isSubscribed": true }),
    );
    let result = set(
        executor,
        json!({ "accountId": mail_account, "create": create }),
    )
    .await?;
    if let Some(id) = result
        .get("created")
        .and_then(|c| c.get(NEW))
        .and_then(|c| c.get("id"))
        .and_then(Value::as_str)
    {
        return resolved(id);
    }
    let Some(error) = result.get("notCreated").and_then(|n| n.get(NEW)) else {
        return Err(
            JmapError::protocol("Mailbox/set neither created nor refused the mailbox").into(),
        );
    };
    if error_type(error) == "alreadyExists" {
        // The op is retried, and a retry meets the folder the first attempt made.
        if let Some(existing) = error.get("existingId").and_then(Value::as_str) {
            return resolved(existing);
        }
        if let Some(existing) = sibling_named(executor, mail_account, parent, name).await? {
            return resolved(&existing);
        }
    }
    Err(refused(NEW, error))
}

/// Sets a mailbox's name and parent: a rename, a move, or a move under Trash.
async fn place(
    executor: &dyn Executor,
    mail_account: &str,
    target: &MailboxId,
    name: &str,
    parent: Option<&MailboxId>,
) -> ProviderResult<MailboxEditReceipt> {
    let mut update = Map::new();
    update.insert(
        target.as_str().to_owned(),
        json!({ "name": name, "parentId": parent.map(MailboxId::as_str) }),
    );
    let result = set(
        executor,
        json!({ "accountId": mail_account, "update": update }),
    )
    .await?;
    if result
        .get("updated")
        .and_then(|u| u.get(target.as_str()))
        .is_some()
    {
        return Ok(MailboxEditReceipt::resolved(target.clone()));
    }
    match result
        .get("notUpdated")
        .and_then(|n| n.get(target.as_str()))
    {
        Some(error) => Err(refused(target.as_str(), error)),
        // Neither applied nor refused: the server dropped the id, never a false success.
        None => Err(JmapError::set(target.as_str(), "notFound").into()),
    }
}

/// Destroys `target` and everything beneath it, deepest first, with their mail.
async fn destroy_subtree(
    executor: &dyn Executor,
    mail_account: &str,
    target: &MailboxId,
) -> ProviderResult<MailboxEditReceipt> {
    let parents = parent_map(executor, mail_account).await?;
    if !parents.contains_key(target.as_str()) {
        // Already gone: the retry of a delete that landed.
        return Ok(MailboxEditReceipt::removed());
    }
    let order = deepest_first(&parents, target.as_str());
    let result = set(
        executor,
        json!({ "accountId": mail_account, "destroy": order, "onDestroyRemoveEmails": true }),
    )
    .await?;
    let destroyed: Vec<&str> = result
        .get("destroyed")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for id in &order {
        if destroyed.contains(&id.as_str()) {
            continue;
        }
        match result.get("notDestroyed").and_then(|n| n.get(id)) {
            Some(error) if error_type(error) == "notFound" => {}
            Some(error) => return Err(refused(id, error)),
            None => return Err(JmapError::set(id.as_str(), "notFound").into()),
        }
    }
    Ok(MailboxEditReceipt::removed())
}

/// `target`'s subtree, each mailbox after every mailbox beneath it.
fn deepest_first(parents: &HashMap<String, Option<String>>, target: &str) -> Vec<String> {
    fn visit(
        parents: &HashMap<String, Option<String>>,
        id: &str,
        seen: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) {
        // A server reporting a cycle must not make this recurse for ever.
        if !seen.insert(id.to_owned()) {
            return;
        }
        let mut children: Vec<&String> = parents
            .iter()
            .filter(|(_, parent)| parent.as_deref() == Some(id))
            .map(|(child, _)| child)
            .collect();
        children.sort();
        for child in children {
            visit(parents, child, seen, order);
        }
        order.push(id.to_owned());
    }
    let mut order = Vec::new();
    visit(parents, target, &mut HashSet::new(), &mut order);
    order
}

/// Every mailbox's parent, by id.
async fn parent_map(
    executor: &dyn Executor,
    mail_account: &str,
) -> ProviderResult<HashMap<String, Option<String>>> {
    Ok(list(executor, mail_account, &["parentId"])
        .await?
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let parent = m.get("parentId").and_then(Value::as_str).map(str::to_owned);
            Some((id, parent))
        })
        .collect())
}

/// The id of the mailbox called `name` directly inside `parent`.
async fn sibling_named(
    executor: &dyn Executor,
    mail_account: &str,
    parent: Option<&MailboxId>,
    name: &str,
) -> ProviderResult<Option<String>> {
    Ok(list(executor, mail_account, &["name", "parentId"])
        .await?
        .iter()
        .find(|m| {
            m.get("name").and_then(Value::as_str) == Some(name)
                && m.get("parentId").and_then(Value::as_str) == parent.map(MailboxId::as_str)
        })
        .and_then(|m| m.get("id")?.as_str().map(str::to_owned)))
}

/// Every mailbox of the account, with `properties`.
async fn list(
    executor: &dyn Executor,
    mail_account: &str,
    properties: &[&str],
) -> ProviderResult<Vec<Value>> {
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let call = req.invoke(
        "Mailbox/get",
        json!({ "accountId": mail_account, "ids": null, "properties": properties }),
    );
    let resp = executor.execute(&req).await?;
    Ok(resp
        .result(&call)?
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Sends one `Mailbox/set` and returns its result arguments.
async fn set(executor: &dyn Executor, arguments: Value) -> ProviderResult<Value> {
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let call = req.invoke("Mailbox/set", arguments);
    let resp = executor.execute(&req).await?;
    Ok(resp.result(&call)?.clone())
}

fn error_type(error: &Value) -> &str {
    error.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Classifies a `SetError` against the folder tree.
///
/// `alreadyExists` and an `invalidProperties` that is about the parent are the tree having
/// moved under the change (a sibling took the name, the destination went, the folder
/// would sit inside itself), so a conflict. An `invalidProperties` naming only `name` is a
/// name this server will not take. Every other type keeps the class RFC 8620 gives it.
fn refused(object: &str, error: &Value) -> ProviderError {
    let kind = error_type(error);
    let about_name_only = error
        .get("properties")
        .and_then(Value::as_array)
        .is_some_and(|p| {
            p.iter().any(|v| v.as_str() == Some("name"))
                && !p.iter().any(|v| v.as_str() == Some("parentId"))
        });
    match kind {
        "alreadyExists" => ProviderError::conflict(format!("{object}: a sibling has that name")),
        "invalidProperties" if about_name_only => {
            ProviderError::invalid_state(format!("{object}: the server refused the name"))
        }
        "invalidProperties" => {
            ProviderError::conflict(format!("{object}: the destination is not a valid parent"))
        }
        _ => JmapError::set(object, kind).into(),
    }
}

fn resolved(id: &str) -> ProviderResult<MailboxEditReceipt> {
    MailboxId::try_from(id)
        .map(MailboxEditReceipt::resolved)
        .map_err(|_| JmapError::protocol("Mailbox/set returned an empty id").into())
}

#[cfg(test)]
#[path = "mailbox_write_tests.rs"]
mod tests;
