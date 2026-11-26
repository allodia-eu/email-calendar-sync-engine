//! Offline tests for how a folder listing gives folders their roles: by the id each well-known
//! alias answers with, never by a display name, which is localized.

use engine_core::mail::MailboxRole;

use super::*;
use crate::test_support::{
    FakeRoute, fake_client_fallible, fake_client_logged, folder_routes, json,
};

/// The four Sync Issues aliases, which the captured personal account answers with a `404`.
const SYNC_ISSUES: [(&str, &str); 4] = [
    (
        "/mailFolders/syncissues",
        include_str!("../tests/fixtures/wellknown/syncissues.json"),
    ),
    (
        "/mailFolders/conflicts",
        include_str!("../tests/fixtures/wellknown/conflicts.json"),
    ),
    (
        "/mailFolders/localfailures",
        include_str!("../tests/fixtures/wellknown/localfailures.json"),
    ),
    (
        "/mailFolders/serverfailures",
        include_str!("../tests/fixtures/wellknown/serverfailures.json"),
    ),
];

/// The personal account's routes, plus a Sync Issues folder and its three children as a
/// Microsoft 365 mailbox answered them: the folder itself among the root's, its aliases
/// resolving, and its `childFolders`.
fn routes_with_sync_issues() -> Vec<(&'static str, FakeRoute)> {
    let mut list = json(include_str!("../tests/fixtures/mail/mailfolders.json"));
    let mut sync_issues = json(include_str!(
        "../tests/fixtures/mail/mailfolder_sync_issues.json"
    ));
    sync_issues
        .as_object_mut()
        .expect("a folder")
        .remove("@odata.context");
    list["value"]
        .as_array_mut()
        .expect("a folder list")
        .push(sync_issues);
    let mut routes: Vec<(&'static str, FakeRoute)> = SYNC_ISSUES
        .iter()
        .map(|(alias, fixture)| (*alias, Ok(json(fixture))))
        .collect();
    routes.push(("/mailFolders?$top", Ok(list)));
    routes.push((
        "/mailFolders/folder-syncissues/childFolders",
        Ok(json(include_str!(
            "../tests/fixtures/mail/mailfolders_sync_issues_children.json"
        ))),
    ));
    let taken: Vec<&str> = routes.iter().map(|(key, _)| *key).collect();
    routes.extend(
        folder_routes()
            .into_iter()
            .filter(|(key, _)| !taken.contains(key)),
    );
    routes
}

#[tokio::test]
async fn the_services_own_folders_get_their_roles_whatever_they_are_called() {
    let mailboxes = folders(&fake_client_fallible(routes_with_sync_issues()))
        .await
        .unwrap();
    let find = |name: &str| mailboxes.iter().find(|m| m.name == name).unwrap();
    // Dutch names, as Exchange gives them to a Dutch mailbox: only the alias's id tells.
    for name in [
        "Synchronisatieproblemen",
        "Conflicten",
        "Lokale fouten",
        "Serverfouten",
    ] {
        assert_eq!(find(name).role, Some(MailboxRole::ServerRecords), "{name}");
    }
    // The stored chats are a role of their own, so a host can tell them from the bookkeeping.
    assert_eq!(
        find("Gesprekgeschiedenis").role,
        Some(MailboxRole::ChatHistory)
    );
    // The three are nested under Sync Issues, which the root holds.
    assert_eq!(find("Synchronisatieproblemen").parent, None);
    assert_eq!(
        find("Conflicten").parent.as_ref().map(MailboxId::as_str),
        Some("folder-syncissues")
    );
    // A folder the user files mail into keeps its own role, and the Outbox still has none.
    assert_eq!(find("Postvak IN").role, Some(MailboxRole::Inbox));
    assert_eq!(find("Postvak UIT").role, None);
}

#[tokio::test]
async fn a_mailbox_without_sync_issues_lists_its_folders_all_the_same() {
    // The captured personal account has no Sync Issues folder, and Graph answers its four
    // aliases `404 ErrorFolderNotFound`: each is skipped, and Conversation History, which the
    // account does have, still gets its role.
    let mailboxes = folders(&fake_client_fallible(folder_routes()))
        .await
        .unwrap();
    let named = |role: MailboxRole| -> Vec<&str> {
        mailboxes
            .iter()
            .filter(|m| m.role.as_ref() == Some(&role))
            .map(|m| m.name.as_str())
            .collect()
    };
    assert_eq!(named(MailboxRole::ServerRecords), Vec::<&str>::new());
    assert_eq!(named(MailboxRole::ChatHistory), ["Gesprekgeschiedenis"]);
}

/// The personal account's routes with one alias answering `status`.
fn routes_answering(alias: &'static str, status: u16) -> Vec<(&'static str, FakeRoute)> {
    let refusal = match status {
        403 => json(include_str!(
            "../tests/fixtures/error/shared_mailbox_access_denied.json"
        )),
        _ => serde_json::json!({ "error": { "code": "InternalServerError" } }),
    };
    let mut routes = vec![(alias, Err((status, refusal)))];
    routes.extend(folder_routes().into_iter().filter(|(key, _)| *key != alias));
    routes
}

#[tokio::test]
async fn a_records_folder_the_caller_may_not_see_is_left_unmarked() {
    // `403 ErrorAccessDenied` on one of these aliases says only that the folder is not the
    // caller's to see: the listing goes on, and the folder, if listed, has no role.
    let mailboxes = folders(&fake_client_fallible(routes_answering(
        "/mailFolders/conversationhistory",
        403,
    )))
    .await
    .unwrap();
    let chats = mailboxes
        .iter()
        .find(|m| m.name == "Gesprekgeschiedenis")
        .unwrap();
    assert_eq!(chats.role, None);
    assert!(mailboxes.iter().any(|m| m.role == Some(MailboxRole::Inbox)));
}

#[tokio::test]
async fn a_failing_server_still_fails_the_listing() {
    // A `500` says nothing about the folder, so a records alias does not swallow it; nor does
    // a standard alias swallow a `403`, which is the caller's grant falling short.
    for (alias, status) in [
        ("/mailFolders/syncissues", 500),
        ("/mailFolders/conversationhistory", 500),
        ("/mailFolders/inbox", 403),
    ] {
        let listed = folders(&fake_client_fallible(routes_answering(alias, status))).await;
        assert!(
            matches!(listed, Err(GraphError::Status { status: got, .. }) if got == status),
            "{alias} {status}"
        );
    }
}

#[tokio::test]
async fn each_alias_is_asked_for_once_and_by_its_id_alone() {
    let (client, _, log) = fake_client_logged(routes_with_sync_issues());
    folders(&client).await.unwrap();
    let urls: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|(_, url, _)| url.clone())
        .collect();
    for alias in ["msgfolderroot"]
        .into_iter()
        .chain(WELL_KNOWN_ROLES.iter().map(|(alias, _)| *alias))
        .chain(RECORD_ROLES.iter().map(|(alias, _)| *alias))
    {
        let asked = format!("/mailFolders/{alias}?$select=id");
        let times = urls.iter().filter(|url| url.ends_with(&asked)).count();
        assert_eq!(times, 1, "{alias}");
    }
    // The root, eleven aliases, the root's folder list, and a `childFolders` page for each
    // folder that counts children: Archiveren and the folder under it, Sync Issues, and
    // Conversation History, whose one child is hidden.
    assert_eq!(WELL_KNOWN_ROLES.len() + RECORD_ROLES.len(), 11);
    assert_eq!(urls.len(), 1 + 11 + 1 + 4);
}
