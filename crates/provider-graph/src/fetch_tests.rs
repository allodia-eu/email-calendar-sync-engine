//! Offline tests for the Graph folder/message fetch and paging: the delta's three entry
//! shapes (full, lightweight partial, removed), the snapshot, and the `$select`/window URLs.

use engine_core::mail::MailboxRole;

use super::*;
use crate::test_support::{fake_client, folder_routes, json, replay_server};

const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");
const CHANGED: &str = include_str!("../tests/fixtures/mail/messages_delta_changed.json");
const CHANGED_FULL: &str = include_str!("../tests/fixtures/mail/messages_delta_changed_full.json");
const REMOVED: &str = include_str!("../tests/fixtures/mail/messages_delta_removed.json");
const DETAIL: &str = include_str!("../tests/fixtures/mail/message_detail.json");
const LIST_P1: &str = include_str!("../tests/fixtures/mail/messages_list_page1.json");
const LIST_P2: &str = include_str!("../tests/fixtures/mail/messages_list_page2.json");

fn inbox() -> MailboxId {
    MailboxId::try_from("folder-inbox").unwrap()
}

#[test]
fn initial_delta_url_windows_by_received_datetime_only_when_since_is_set() {
    let client = fake_client(vec![]);
    let since = CalendarDate::new(2026, 4, 1).unwrap();

    // The initial request carries the `receivedDateTime` window (spaces encoded).
    let windowed = page_url(&client, &inbox(), None, None, Some(since));
    assert!(windowed.contains("/messages/delta?$select="));
    assert!(windowed.contains("&$filter=receivedDateTime%20ge%202026-04-01T00:00:00Z"));

    // No `since` → no filter (whole folder).
    assert!(!page_url(&client, &inbox(), None, None, None).contains("$filter"));

    // A continuation cursor is followed verbatim — the deltaLink already carries the
    // window, so the filter is never re-appended (which Graph would reject).
    let cursor = SyncState::new(
        "https://graph.microsoft.com/v1.0/me/mailFolders('inbox')/messages/delta?$deltatoken=x",
    );
    let followed = page_url(&client, &inbox(), Some(&cursor), None, Some(since));
    assert_eq!(followed, cursor.as_str());
    assert!(!followed.contains("$filter"));
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
    let first = messages_page(&client, &inbox(), None, None, None)
        .await
        .unwrap();
    assert_eq!(first.changed.len(), 1);
    // Following the real nextLink reaches page 2 — proving continuation works.
    let token = first.next_page.expect("a nextLink continuation");
    let second = messages_page(&client, &inbox(), None, Some(&token), None)
        .await
        .unwrap();
    assert_eq!(second.changed.len(), 1);
}

#[tokio::test]
async fn a_lightweight_partial_resolves_to_state_and_removed_tombstones() {
    let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
    // A lightweight `isRead`-only change is a partial (no @odata.etag). It resolves
    // through the **narrow** `$select` — the route key is that select, so a whole
    // -message re-fetch would find no route and error.
    let client = fake_client(vec![
        ("delta-token-1", json(CHANGED)),
        (
            "$select=id,isRead,isDraft,flag,lastModifiedDateTime,changeKey",
            serde_json::json!({
                "@odata.etag": "W/\"CQAAABYAAAD3\"",
                "id": "message-3",
                "isRead": true,
                "isDraft": false,
                "flag": { "flagStatus": "notFlagged" },
                "lastModifiedDateTime": "2026-03-18T10:00:00Z",
                "changeKey": "CQAAABYAAAD3",
            }),
        ),
    ]);
    let page = messages_page(&client, &inbox(), Some(&cursor), None, None)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(
        page.changed.is_empty(),
        "an isRead change rewrites no message"
    );
    assert_eq!(page.patched.len(), 1);
    assert_eq!(page.patched[0].key.as_str(), "message-3");
    assert!(
        page.patched[0]
            .state
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(engine_core::mail::SystemKeyword::Seen))
    );
    // The tokens ride along: a state change replaces the row's, so one that left
    // them empty would blank the changeKey a later conditional write quotes.
    assert!(page.patched[0].state.revisions.change_key.is_some());
    assert!(page.patched[0].state.last_modified.is_some());
    assert!(page.present.is_empty()); // a delta carries no present set
    assert!(page.removed.is_empty());

    // A removed entry → an inline tombstone, no re-fetch.
    let client = fake_client(vec![("delta-token-1", json(REMOVED))]);
    let page = messages_page(&client, &inbox(), Some(&cursor), None, None)
        .await
        .unwrap();
    assert_eq!(page.removed.len(), 1);
    assert!(page.changed.is_empty());
    assert!(page.next_cursor.as_str().contains("deltatoken"));
}

#[tokio::test]
async fn a_snapshot_entry_without_an_etag_is_still_a_whole_message() {
    // A snapshot's `present` set drives end-of-pass tombstoning, and a state change
    // carries no key into it — so routing one here would delete the message it was
    // reporting on. Snapshot entries are full objects; this proves the branch is
    // reachable only from a delta.
    let snapshot = serde_json::json!({
        "@odata.deltaLink": "https://graph.test/me/mailFolders('inbox')/messages/delta?$deltatoken=t",
        "value": [{ "id": "message-3", "isRead": true, "parentFolderId": "folder-inbox" }],
    });
    let client = fake_client(vec![
        ("messages/delta", snapshot),
        ("/me/messages/", json(DETAIL)),
    ]);
    let page = messages_page(&client, &inbox(), None, None, None)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert!(page.patched.is_empty());
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.present.len(), 1, "and it is covered by the pass");
}

#[tokio::test]
async fn incremental_delta_uses_a_full_changed_entry_without_refetch() {
    // A substantive change returns a FULL object (with @odata.etag), so it is used
    // directly — no `/me/messages/` re-fetch route is provided, so a re-fetch
    // would error; the test succeeding proves none happens. This is the doc's
    // "changed entries are full objects" common case.
    let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
    let client = fake_client(vec![("delta-token-1", json(CHANGED_FULL))]);
    let page = messages_page(&client, &inbox(), Some(&cursor), None, None)
        .await
        .unwrap();
    assert_eq!(page.changed.len(), 1);
    assert!(page.changed[0].envelope.subject.is_some());
    assert!(page.changed[0].revisions.etag.is_some());
}

#[tokio::test]
async fn a_response_without_a_value_array_is_a_protocol_error() {
    let client = fake_client(vec![("messages/delta", json(r#"{"unexpected":true}"#))]);
    assert!(
        messages_page(&client, &inbox(), None, None, None)
            .await
            .is_err()
    );
    // An unrouted request surfaces the fake's error rather than hanging.
    assert!(
        messages_page(&fake_client(vec![]), &inbox(), None, None, None)
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
    let client =
        GraphClient::with_base("t", replay_server(routes), crate::test_support::tls()).unwrap();
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
        "/users/info@example.org/mailFolders/msgfolderroot",
        json(include_str!(
            "../tests/fixtures/wellknown/msgfolderroot.json"
        )),
    ));
    let client = fake_client(routes).with_principal(MailboxPrincipal::user("info@example.org"));
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
    let client = GraphClient::with_base(
        "t",
        replay_server(vec![("$deltatoken=", json(CHANGED))]),
        crate::test_support::tls(),
    )
    .unwrap();
    let cursor = SyncState::new(
        "https://graph.microsoft.com/v1.0/me/mailFolders('inbox')/messages/delta?$deltatoken=x",
    );
    let page = messages_page(&client, &inbox(), Some(&cursor), None, None)
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
        messages_page(&client, &inbox(), Some(&cursor), None, None)
            .await
            .is_err()
    );
}
