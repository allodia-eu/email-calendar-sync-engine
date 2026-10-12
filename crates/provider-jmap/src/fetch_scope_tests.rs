//! A whole-scope sync stays inside the server's `maxObjectsInGet`, however large the scope.
//!
//! Driven by [`BoundedServer`], which refuses an oversized `/get` the way a real server does,
//! so each test fails on code that asks for more than one request may carry.

use std::{collections::BTreeSet, sync::Arc};

use engine_core::{
    error::FailureClass,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider, Provider};

use super::{
    JmapProvider,
    bounded_server::{BoundedServer, card, event, mailbox},
    provider_test_support::account,
};

const LIMIT: usize = 500;

fn provider_over(server: &Arc<BoundedServer>) -> JmapProvider {
    JmapProvider::with_executor(Box::new(server.clone()))
}

fn keys<T: engine_core::sync::Keyed>(objects: &[T]) -> BTreeSet<String> {
    objects
        .iter()
        .map(|object| object.provider_key().as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn an_event_snapshot_larger_than_one_get_arrives_whole() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("CalendarEvent", (0..1203).map(event));

    let sync = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a complete pass is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
    assert_eq!(present.len(), 1203);
    assert_eq!(sync.next_cursor.as_str(), server.state("CalendarEvent"));
    assert_eq!(server.request_count(), 3, "three pages of at most {LIMIT}");
}

#[tokio::test]
async fn a_server_that_caps_its_query_is_paged_to_the_end() {
    let server = Arc::new(BoundedServer::new(LIMIT).with_query_cap(100));
    server.seed("CalendarEvent", (0..250).map(event));

    let sync = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("a complete pass is a snapshot");
    };
    assert_eq!(objects.len(), 250);
}

/// Destroying an object on an earlier page moves every later id up one position, so a pass that
/// paged on would skip an object no later delta mentions. The pass is taken again instead, and
/// arrives as a snapshot of what is there now.
#[tokio::test]
async fn a_snapshot_whose_query_moved_mid_pass_is_taken_again() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("CalendarEvent", (0..1203).map(event));
    server.change_after(0, "CalendarEvent", |store| store.destroy("e00000"));

    let sync = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a pass taken again on a settled query is a snapshot");
    };
    assert_eq!(objects.len(), 1202);
    assert_eq!(present.len(), 1202);
    assert!(!keys(objects).contains("e00000"));
    assert!(keys(objects).contains("e00500"), "the id that moved pages");
    assert_eq!(sync.next_cursor.as_str(), server.state("CalendarEvent"));
    assert_eq!(server.request_count(), 6, "the pass, then the pass again");
}

/// A query that moves under every pass has no complete answer to store, so the pass fails as one
/// to retry rather than storing part of the scope.
#[tokio::test]
async fn a_snapshot_that_never_settles_fails_the_pass_for_a_retry() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("CalendarEvent", (0..1203).map(event));
    for request in 0..20 {
        server.change_after(request, "CalendarEvent", move |store| {
            store.upsert(event(5000 + request));
        });
    }

    let error = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap_err();

    assert_eq!(error.class(), FailureClass::Retryable, "{error}");
    assert_eq!(server.request_count(), 9, "three passes of three pages");
}

#[tokio::test]
async fn a_mailbox_tree_larger_than_one_get_arrives_whole() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("Mailbox", (0..1203).map(mailbox));

    let sync = provider_over(&server)
        .sync_mailboxes(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a complete pass is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
    assert_eq!(present.len(), 1203);
    assert_eq!(server.request_count(), 3, "three pages of at most {LIMIT}");
}

#[tokio::test]
async fn an_event_delta_larger_than_one_get_arrives_whole() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("CalendarEvent", (0..10).map(event));
    let since = SyncState::new(server.state("CalendarEvent"));
    server.change("CalendarEvent", |store| {
        (10..1210).for_each(|n| store.upsert(event(n)));
        store.destroy("e00003");
    });

    let sync = provider_over(&server)
        .sync_events(&account(), Some(&since))
        .await
        .unwrap();

    let SyncUpdate::Delta {
        changed, removed, ..
    } = &sync.update
    else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(changed.len(), 1200);
    assert_eq!(removed.len(), 1);
    assert_eq!(sync.next_cursor.as_str(), server.state("CalendarEvent"));
}

/// Paged changes are applied in order: an object created on one page and destroyed on a later
/// one is gone, and one destroyed and then created again is present.
#[tokio::test]
async fn a_paged_delta_keeps_the_last_word_on_each_object() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("CalendarEvent", (0..2).map(event));
    let since = SyncState::new(server.state("CalendarEvent"));
    server.change("CalendarEvent", |store| {
        store.upsert(event(5000));
        store.destroy("e00001");
        (10..610).for_each(|n| store.upsert(event(n)));
        store.destroy("e05000");
        store.upsert(event(1));
    });

    let sync = provider_over(&server)
        .sync_events(&account(), Some(&since))
        .await
        .unwrap();

    let SyncUpdate::Delta {
        changed, removed, ..
    } = &sync.update
    else {
        panic!("a cursor gives a delta");
    };
    let changed = keys(changed);
    let removed: BTreeSet<_> = removed.iter().map(|key| key.as_str().to_owned()).collect();
    assert!(!changed.contains("e05000") && removed.contains("e05000"));
    assert!(changed.contains("e00001") && !removed.contains("e00001"));
    assert_eq!(changed.len(), 601);
}

#[tokio::test]
async fn a_contact_snapshot_larger_than_one_get_arrives_whole() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("ContactCard", (0..1203).map(card));

    let ContactSourceSync::Available { sync, .. } = provider_over(&server)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("the server has contacts");
    };

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a complete pass is a snapshot");
    };
    assert_eq!((objects.len(), present.len()), (1203, 1203));
}

#[tokio::test]
async fn a_contact_delta_larger_than_one_get_arrives_whole() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    let since = SyncState::new(server.state("ContactCard"));
    server.change("ContactCard", |store| {
        (0..1203).for_each(|n| store.upsert(card(n)));
    });

    let ContactSourceSync::Available { sync, .. } = provider_over(&server)
        .sync_contacts(&account(), Some(&since))
        .await
        .unwrap()
    else {
        panic!("the server has contacts");
    };

    let SyncUpdate::Delta { changed, .. } = &sync.update else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(changed.len(), 1203);
}

#[tokio::test]
async fn a_container_delta_larger_than_one_get_arrives_whole() {
    let server = Arc::new(BoundedServer::new(LIMIT));
    let since = SyncState::new(server.state("Calendar"));
    server.change("Calendar", |store| {
        (0..1203).for_each(|n| {
            store
                .upsert(serde_json::json!({ "id": format!("cal{n:05}"), "name": format!("C{n}") }));
        });
    });

    let sync = provider_over(&server)
        .sync_calendars(&account(), Some(&since))
        .await
        .unwrap();

    let SyncUpdate::Delta { changed, .. } = &sync.update else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(changed.len(), 1203);
    assert_eq!(sync.next_cursor.as_str(), server.state("Calendar"));
}

/// A server that keeps saying there is more without moving its state would page forever, so the
/// pass recovers with a snapshot as it would from `cannotCalculateChanges`.
#[tokio::test]
async fn more_changes_without_a_new_state_recovers_with_a_snapshot() {
    let stuck = serde_json::json!({ "methodResponses": [
        ["CalendarEvent/changes", { "accountId": "c", "oldState": "s1", "newState": "s1",
            "hasMoreChanges": true, "created": [], "updated": [], "destroyed": [] }, "0"],
        ["CalendarEvent/get", { "accountId": "c", "state": "s1", "list": [] }, "1"],
        ["CalendarEvent/get", { "accountId": "c", "state": "s1", "list": [] }, "2"]
    ] });
    let snapshot = serde_json::json!({ "methodResponses": [
        ["CalendarEvent/query", { "accountId": "c", "queryState": "q1", "ids": [],
            "position": 0, "total": 0, "canCalculateChanges": false }, "0"],
        ["CalendarEvent/get", { "accountId": "c", "state": "s2", "list": [] }, "1"]
    ] });
    let (provider, exec) = super::provider_test_support::recording(vec![stuck, snapshot]);

    let sync = provider
        .sync_events(&account(), Some(&SyncState::new("s1")))
        .await
        .unwrap();

    assert!(sync.update.is_snapshot(), "{:?}", sync.update);
    assert_eq!(sync.next_cursor.as_str(), "s2");
    assert_eq!(exec.request_count(), 2);
}
