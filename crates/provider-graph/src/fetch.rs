//! Folder-list and message snapshot/delta fetch + paging for the Graph provider.
//!
//! Two passes feed [`messages_page`]:
//! - **snapshot** (`cursor` `None`): the initial `messages/delta` enumeration
//!   returns *full* objects; each becomes a `changed` + `present` entry, and the
//!   pass ends at the `@odata.deltaLink` (the cursor to persist).
//! - **incremental delta** (`cursor` `Some`): a changed entry is normally a *full*
//!   object (it carries `@odata.etag`) and is used directly; a *lightweight* change
//!   (e.g. `isRead`) returns an etag-less *partial*, which is **re-fetched** as a
//!   full message ([`message`]). `@removed` tombstones apply inline. Multi-page
//!   passes follow `@odata.nextLink`.

use engine_core::ids::{MailboxId, MessageId, ProviderKey};
use engine_core::mail::{Mailbox, Message};
use engine_core::sync::SyncState;
use engine_provider::{PageToken, SyncKind, SyncPage};
use serde_json::Value;

use crate::error::GraphError;
use crate::json::{req_str, wrap_id};
use crate::normalize::{
    MESSAGE_SELECT, WELL_KNOWN_ROLES, apply_roles, folder_from_json, message_from_json,
    well_known_folder_id,
};
use crate::transport::GraphClient;

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
        .get(&client.url(&format!("/messages/{}?$select={select}", id.as_str())))
        .await?;
    message_from_json(&doc)
}

/// Fetches one page of the bound folder's messages (see the module docs).
pub(crate) async fn messages_page(
    client: &GraphClient,
    folder: &MailboxId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
) -> Result<SyncPage<Message>, GraphError> {
    let kind = if cursor.is_none() {
        SyncKind::Snapshot
    } else {
        SyncKind::Delta
    };
    let doc = client.get(&page_url(client, folder, cursor, page)).await?;

    let mut changed = Vec::new();
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
        // mailboxes), which returns only the changed property + id with no etag; those
        // (and never a full entry) are re-fetched. Snapshot entries are always full.
        let full = if entry.get("@odata.etag").is_some() {
            message_from_json(entry)?
        } else {
            let id = MessageId::new(entry_key(entry)?);
            match message(client, &id).await {
                Ok(full) => full,
                // Deleted/moved in the race since the delta → skip; a later delta
                // reports the removal, so the pass is not wedged.
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
        removed,
        present,
        next_page,
        next_cursor,
        total: None,
    })
}

/// The URL for the next page: a continuation `@odata.nextLink`, else the delta
/// `cursor` (an `@odata.deltaLink`), else the folder's first `messages/delta` call.
fn page_url(
    client: &GraphClient,
    folder: &MailboxId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
) -> String {
    if let Some(page) = page {
        page.as_str().to_owned()
    } else if let Some(cursor) = cursor {
        cursor.as_str().to_owned()
    } else {
        let select = MESSAGE_SELECT.join(",");
        client.url(&format!(
            "/mailFolders/{}/messages/delta?$select={select}",
            folder.as_str()
        ))
    }
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
mod tests {
    use super::*;
    use crate::test_support::{fake_client, folder_routes, json, replay_server};
    use engine_core::mail::MailboxRole;

    const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");
    const CHANGED: &str = include_str!("../tests/fixtures/mail/messages_delta_changed.json");
    const CHANGED_FULL: &str =
        include_str!("../tests/fixtures/mail/messages_delta_changed_full.json");
    const REMOVED: &str = include_str!("../tests/fixtures/mail/messages_delta_removed.json");
    const DETAIL: &str = include_str!("../tests/fixtures/mail/message_detail.json");
    const LIST_P1: &str = include_str!("../tests/fixtures/mail/messages_list_page1.json");
    const LIST_P2: &str = include_str!("../tests/fixtures/mail/messages_list_page2.json");

    fn inbox() -> MailboxId {
        MailboxId::try_from("folder-inbox").unwrap()
    }

    #[tokio::test]
    async fn folders_resolve_roles_by_id_and_null_root_parents() {
        let mailboxes = folders(&fake_client(folder_routes())).await.unwrap();
        assert_eq!(mailboxes.len(), 8);
        assert!(mailboxes.iter().all(|m| m.parent.is_none()));
        let role = |name: &str| {
            mailboxes
                .iter()
                .find(|m| m.name == name)
                .unwrap()
                .role
                .clone()
        };
        assert_eq!(role("Postvak IN"), Some(MailboxRole::Inbox));
        assert_eq!(role("Verzonden items"), Some(MailboxRole::Sent));
        assert_eq!(role("Postvak UIT"), None);
    }

    #[tokio::test]
    async fn snapshot_page_yields_full_objects_and_a_delta_cursor() {
        let page = messages_page(
            &fake_client(vec![("messages/delta", json(SNAPSHOT))]),
            &inbox(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(page.kind, SyncKind::Snapshot);
        assert_eq!(page.changed.len(), 3);
        assert_eq!(page.present.len(), 3);
        assert!(page.removed.is_empty());
        assert!(page.next_page.is_none());
        // The pass ends at the deltaLink, which becomes the persisted cursor.
        assert!(page.next_cursor.as_str().contains("deltatoken"));
    }

    #[tokio::test]
    async fn snapshot_follows_nextlink_across_pages() {
        let (p1, p2) = (json(LIST_P1), json(LIST_P2));
        let next = p1.get("@odata.nextLink").and_then(Value::as_str).unwrap();
        // The client rebases the absolute nextLink onto its base, so route on the
        // path that survives rebasing (everything after the Graph root).
        let next_path = next
            .strip_prefix("https://graph.microsoft.com/v1.0")
            .unwrap_or(next);
        // Page 1 from the initial call; page 2 from following the real nextLink.
        let client = fake_client(vec![("messages/delta", p1.clone()), (next_path, p2)]);
        let first = messages_page(&client, &inbox(), None, None).await.unwrap();
        assert_eq!(first.changed.len(), 1);
        // Following the real nextLink reaches page 2 — proving continuation works.
        let token = first.next_page.expect("a nextLink continuation");
        let second = messages_page(&client, &inbox(), None, Some(&token))
            .await
            .unwrap();
        assert_eq!(second.changed.len(), 1);
    }

    #[tokio::test]
    async fn incremental_delta_refetches_a_lightweight_partial_and_tombstones_removed() {
        let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
        // A lightweight `isRead`-only change is a partial (no @odata.etag) → re-fetch
        // the full message (id != "delta-token-1").
        let client = fake_client(vec![
            ("delta-token-1", json(CHANGED)),
            ("/me/messages/", json(DETAIL)),
        ]);
        let page = messages_page(&client, &inbox(), Some(&cursor), None)
            .await
            .unwrap();
        assert_eq!(page.kind, SyncKind::Delta);
        assert_eq!(page.changed.len(), 1);
        assert!(page.present.is_empty()); // a delta carries no present set
        assert!(page.removed.is_empty());

        // A removed entry → an inline tombstone, no re-fetch.
        let client = fake_client(vec![("delta-token-1", json(REMOVED))]);
        let page = messages_page(&client, &inbox(), Some(&cursor), None)
            .await
            .unwrap();
        assert_eq!(page.removed.len(), 1);
        assert!(page.changed.is_empty());
        assert!(page.next_cursor.as_str().contains("deltatoken"));
    }

    #[tokio::test]
    async fn incremental_delta_uses_a_full_changed_entry_without_refetch() {
        // A substantive change returns a FULL object (with @odata.etag), so it is used
        // directly — no `/me/messages/` re-fetch route is provided, so a re-fetch
        // would error; the test succeeding proves none happens. This is the doc's
        // "changed entries are full objects" common case.
        let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
        let client = fake_client(vec![("delta-token-1", json(CHANGED_FULL))]);
        let page = messages_page(&client, &inbox(), Some(&cursor), None)
            .await
            .unwrap();
        assert_eq!(page.changed.len(), 1);
        assert!(page.changed[0].envelope.subject.is_some());
        assert!(page.changed[0].revisions.etag.is_some());
    }

    #[tokio::test]
    async fn a_response_without_a_value_array_is_a_protocol_error() {
        let client = fake_client(vec![("messages/delta", json(r#"{"unexpected":true}"#))]);
        assert!(messages_page(&client, &inbox(), None, None).await.is_err());
        // An unrouted request surfaces the fake's error rather than hanging.
        assert!(
            messages_page(&fake_client(vec![]), &inbox(), None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn folders_drain_every_page_of_the_list() {
        // A folder list paginated across two pages (`@odata.nextLink`) is fully
        // drained, not truncated at the first page.
        let page1 = serde_json::json!({
            "value": [{ "id": "folder-a", "displayName": "A", "parentFolderId": "folder-root" }],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/mailFolders?$skiptoken=PAGE2"
        });
        let page2 = serde_json::json!({
            "value": [{ "id": "folder-b", "displayName": "B", "parentFolderId": "folder-root" }]
        });
        let mut routes: Vec<(&str, Value)> = folder_routes()
            .into_iter()
            .filter(|(key, _)| *key != "/mailFolders?$top")
            .collect();
        routes.push(("mailFolders?$top", page1));
        routes.push(("skiptoken=PAGE2", page2));
        let mailboxes = folders(&fake_client(routes)).await.unwrap();
        assert_eq!(mailboxes.len(), 2);
    }

    #[tokio::test]
    async fn folders_skip_an_unprovisioned_well_known_alias() {
        // The `archive` alias is unrouted → the replay server 404s it → its role is
        // skipped, and the rest of the folder list still syncs.
        let routes: Vec<(&str, Value)> = folder_routes()
            .into_iter()
            .filter(|(key, _)| *key != "/mailFolders/archive")
            .collect();
        let client = GraphClient::with_base("t", replay_server(routes)).unwrap();
        let mailboxes = folders(&client).await.unwrap();
        // The Archive folder is present but roleless (its alias 404'd); a
        // provisioned alias still resolved.
        let archive = mailboxes.iter().find(|m| m.name == "Archiveren").unwrap();
        assert!(archive.role.is_none());
        assert!(mailboxes.iter().any(|m| m.role == Some(MailboxRole::Inbox)));
    }

    #[tokio::test]
    async fn folders_address_a_shared_mailbox() {
        use crate::principal::MailboxPrincipal;
        // The first request (msgfolderroot) is routed ONLY under the
        // /users/{address} prefix, so the whole folder sync succeeds only if the
        // principal roots the URLs there — proving a shared mailbox is reachable.
        let mut routes: Vec<(&str, Value)> = folder_routes()
            .into_iter()
            .filter(|(key, _)| *key != "/mailFolders/msgfolderroot")
            .collect();
        routes.push((
            "/users/info@company.org/mailFolders/msgfolderroot",
            json(include_str!(
                "../tests/fixtures/wellknown/msgfolderroot.json"
            )),
        ));
        let client = fake_client(routes).with_principal(MailboxPrincipal::user("info@company.org"));
        assert!(folders(&client).await.is_ok());
    }

    #[tokio::test]
    async fn folders_propagate_a_non_404_alias_failure() {
        // A non-404 failure on an alias (here the fake's protocol error for an
        // unrouted alias) is propagated, not silently skipped like a 404.
        let routes: Vec<(&str, Value)> = folder_routes()
            .into_iter()
            .filter(|(key, _)| *key != "/mailFolders/inbox")
            .collect();
        assert!(folders(&fake_client(routes)).await.is_err());
    }

    #[tokio::test]
    async fn delta_refetch_skips_a_message_that_404s() {
        // The partial change re-fetch is unrouted on the replay server → 404 → the
        // change is skipped (a later delta reports the removal), not propagated.
        let client =
            GraphClient::with_base("t", replay_server(vec![("$deltatoken=", json(CHANGED))]))
                .unwrap();
        let cursor = SyncState::new(
            "https://graph.microsoft.com/v1.0/me/mailFolders('inbox')/messages/delta?$deltatoken=x",
        );
        let page = messages_page(&client, &inbox(), Some(&cursor), None)
            .await
            .unwrap();
        assert!(page.changed.is_empty());
    }

    #[tokio::test]
    async fn delta_refetch_propagates_a_non_404_failure() {
        // A non-404 re-fetch failure (the fake's protocol error for the unrouted
        // message GET) is propagated, not swallowed.
        let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
        let client = fake_client(vec![("delta-token-1", json(CHANGED))]);
        assert!(
            messages_page(&client, &inbox(), Some(&cursor), None)
                .await
                .is_err()
        );
    }
}
