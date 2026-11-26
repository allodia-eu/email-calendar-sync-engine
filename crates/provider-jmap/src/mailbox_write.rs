//! Changing the account's folder tree via `Mailbox/set` (RFC 8621 §2.5).
//!
//! A JMAP mailbox id is a server id that survives a rename and a move, so every edit but a
//! create resolves to the id it named:
//!
//! - [`MailboxEdit::Create`] → one `create` with `isSubscribed: true`, so the folder shows in
//!   clients that list subscriptions only, and the `role` asked for, where one is: a registered
//!   role ([`MailboxRole::jmap_role`]), any other refused before sending.
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
//!
//! A create with a role the server refuses is told apart by the tree, never by the server's
//! prose: RFC 8621 §2 allows one mailbox per role, so the refusal is read against the folder
//! list. The folder asked for, already there with the role, is the retry of a create that
//! landed; a folder of that name without the role, or another folder holding the role, is a
//! conflict; a role nobody holds that the server still refuses is one it will not give.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
};
use engine_provider::{
    Capabilities, CreateRoleError, MailboxEdit, MailboxEditReceipt, MailboxWrites, Provider,
    ProviderError, ProviderResult,
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
        let capabilities = self.connection_info().capabilities;
        edit_mailbox(self.executor.as_ref(), &account, edit, &capabilities).await
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
    capabilities: &Capabilities,
) -> ProviderResult<MailboxEditReceipt> {
    let role = edit.role_as(capabilities, MailboxRole::jmap_role)?;
    match edit {
        MailboxEdit::Create { name, parent, .. } => {
            create(executor, mail_account, name, parent.as_ref(), role).await
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
    role: Option<&str>,
) -> ProviderResult<MailboxEditReceipt> {
    let mut mailbox =
        json!({ "name": name, "parentId": parent.map(MailboxId::as_str), "isSubscribed": true });
    if let Some(role) = role {
        mailbox["role"] = json!(role);
    }
    let mut create = Map::new();
    create.insert(NEW.to_owned(), mailbox);
    let result = set(
        executor,
        json!({ "accountId": mail_account, "create": create }),
    )
    .await?;
    if let Some(created) = result.get("created").and_then(|c| c.get(NEW))
        && let Some(id) = created.get("id").and_then(Value::as_str)
    {
        if let Some(role) = role {
            kept_role(executor, mail_account, id, created, role).await?;
        }
        return resolved(id);
    }
    let Some(error) = result.get("notCreated").and_then(|n| n.get(NEW)) else {
        return Err(
            JmapError::protocol("Mailbox/set neither created nor refused the mailbox").into(),
        );
    };
    if let Some(role) = role {
        let about_role = names_property(error, "role");
        if about_role || error_type(error) == "alreadyExists" {
            let tree = list(executor, mail_account, &["name", "parentId", "role"]).await?;
            let existing = error.get("existingId").and_then(Value::as_str);
            if let Some(settled) = role_refusal(&tree, existing, name, parent, role) {
                return settled;
            }
            if about_role {
                return Err(ProviderError::invalid_state(
                    "the server will not give a folder that role",
                ));
            }
        }
        return Err(refused(NEW, error));
    }
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

/// Confirms that the mailbox a create with `role` made has it: from `created` where the server
/// echoed it, else from one `Mailbox/get` of that mailbox. A server is to return only what it
/// set or changed (RFC 8620 §5.3), so the role is usually not in `created`.
///
/// # Errors
///
/// [`CreateRoleError::NotKept`], naming the mailbox, where it has another role or none (or is
/// not there to read): the server took the create and dropped the role. The mailbox is left as
/// the server made it.
async fn kept_role(
    executor: &dyn Executor,
    mail_account: &str,
    id: &str,
    created: &Value,
    role: &str,
) -> ProviderResult<()> {
    let held = if let Some(echoed) = created.get("role") {
        echoed.as_str().map(str::to_owned)
    } else {
        let mut req = Request::new([capability::CORE, capability::MAIL]);
        let call = req.invoke(
            "Mailbox/get",
            json!({ "accountId": mail_account, "ids": [id], "properties": ["role"] }),
        );
        let resp = executor.execute(&req).await?;
        resp.result(&call)?
            .get("list")
            .and_then(Value::as_array)
            .and_then(|list| list.first())
            .and_then(|mailbox| mailbox.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    if held.is_some_and(|held| held.eq_ignore_ascii_case(role)) {
        return Ok(());
    }
    // The one refusal with a side effect: the mailbox is there, without the role.
    let mailbox = MailboxId::try_from(id)
        .map_err(|_| JmapError::protocol("Mailbox/set returned an empty id"))?;
    Err(CreateRoleError::NotKept { mailbox }.into())
}

/// What `tree` says about a create of `name` in `parent` with `role` that the server refused,
/// or `None` where it explains nothing.
///
/// The folder the refusal names (`existingId`) is looked at first, since a server may have
/// normalised the name; else the one by that name in that parent. Holding the role, it is the
/// one asked for: an earlier attempt of this create made it. Without it, it is someone else's
/// folder, and a create that asked for a role does not resolve to it. Another folder holding
/// the role is the one RFC 8621 §2 allows. Roles are compared ignoring case, as
/// `from_jmap_role` reads them.
fn role_refusal(
    tree: &[Value],
    existing: Option<&str>,
    name: &str,
    parent: Option<&MailboxId>,
    role: &str,
) -> Option<ProviderResult<MailboxEditReceipt>> {
    let holds_role = |m: &&Value| {
        m.get("role")
            .and_then(Value::as_str)
            .is_some_and(|held| held.eq_ignore_ascii_case(role))
    };
    let by_id = existing.and_then(|id| {
        tree.iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(id))
    });
    let named = by_id.or_else(|| {
        tree.iter().find(|m| {
            m.get("name").and_then(Value::as_str) == Some(name)
                && m.get("parentId").and_then(Value::as_str) == parent.map(MailboxId::as_str)
        })
    });
    match named {
        Some(folder) if holds_role(&folder) => Some(resolved(
            folder.get("id").and_then(Value::as_str).unwrap_or_default(),
        )),
        Some(_) => Some(Err(ProviderError::conflict(
            "a folder of that name is already there without that role",
        ))),
        None => tree.iter().find(holds_role).map(|_| {
            Err(ProviderError::conflict(
                "another folder already has that role",
            ))
        }),
    }
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

/// Whether a `SetError` names `property` among the ones it refused.
fn names_property(error: &Value, property: &str) -> bool {
    error
        .get("properties")
        .and_then(Value::as_array)
        .is_some_and(|p| p.iter().any(|v| v.as_str() == Some(property)))
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
        _ => JmapError::refused(object, error).into(),
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
