//! A snapshot walked while another client changes the account misses nothing and leaves nothing
//! behind.
//!
//! Driven by [`BoundedServer`], whose query honours `anchor` the way a server does and whose data
//! a test changes between two requests, as another device would mid-sync.

use std::{collections::BTreeSet, sync::Arc};

use engine_core::{
    error::FailureClass,
    sync::{Keyed, SyncState, SyncUpdate},
};
use engine_provider::Provider;
use serde_json::Value;

use super::{
    JmapProvider,
    bounded_server::{BoundedServer, email, event},
    provider_test_support::account,
};
use crate::walk::anchors;

const LIMIT: usize = 500;

fn provider_over(server: &Arc<BoundedServer>) -> JmapProvider {
    JmapProvider::with_executor(Box::new(server.clone()))
}

fn keys<T: Keyed>(objects: &[T]) -> BTreeSet<String> {
    objects
        .iter()
        .map(|object| object.provider_key().as_str().to_owned())
        .collect()
}

fn mailbox_of(n: usize) -> Arc<BoundedServer> {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("Email", (0..n).map(email));
    server
}

fn with_id(mut object: Value, id: &str) -> Value {
    object["id"] = Value::from(id);
    object
}

/// Another device deletes a message the new one has already read. Paging by position would move
/// every later message up one place and skip the one that opened the next page; the walk is
/// anchored, so nothing is skipped, and the deleted message leaves with the next delta.
#[tokio::test]
async fn a_message_deleted_on_a_page_already_read_skips_nothing() {
    let server = mailbox_of(1203);
    let before = server.state("Email");
    server.change_after(0, "Email", |store| store.destroy("x00000"));
    let provider = provider_over(&server);

    let sync = provider.sync_email(&account(), None).await.unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
    assert_eq!(present.len(), 1203);
    assert!(
        keys(objects).contains("x00500"),
        "the message that opened page two"
    );
    assert_eq!(
        sync.next_cursor.as_str(),
        before,
        "the state read with the first page"
    );
    assert_eq!(server.request_count(), 4, "three pages and a listing");

    let delta = provider
        .sync_email(&account(), Some(&sync.next_cursor))
        .await
        .unwrap();
    let SyncUpdate::Delta { removed, .. } = &delta.update else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(
        removed
            .iter()
            .map(engine_core::ids::ProviderKey::as_str)
            .collect::<Vec<_>>(),
        ["x00000"]
    );
}

/// The message the next page was to start after is the one deleted: the server answers
/// `anchorNotFound`, and the walk resumes after the message before it.
#[tokio::test]
async fn a_deleted_anchor_steps_back_to_the_message_before_it() {
    let server = mailbox_of(1203);
    server.change_after(0, "Email", |store| store.destroy("x00499"));

    let sync = provider_over(&server)
        .sync_email(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
    assert_eq!(
        server.request_count(),
        5,
        "three pages, one refused anchor, and a listing"
    );
}

/// Mail arriving above the walk is not lost: the closing listing finds it, and the next delta
/// names it too, which the store applies as the same message.
#[tokio::test]
async fn mail_arriving_during_the_walk_is_not_lost() {
    let server = mailbox_of(1203);
    server.change_after(0, "Email", |store| {
        store.upsert(with_id(email(0), "x00000new"));
    });
    let provider = provider_over(&server);

    let sync = provider.sync_email(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert!(keys(objects).contains("x00000new"));

    let delta = provider
        .sync_email(&account(), Some(&sync.next_cursor))
        .await
        .unwrap();
    let SyncUpdate::Delta { changed, .. } = &delta.update else {
        panic!("a cursor gives a delta");
    };
    assert!(keys(changed).contains("x00000new"));
}

/// A flag set on a message after it was read arrives with the next delta.
#[tokio::test]
async fn a_flag_set_during_the_walk_arrives_with_the_next_delta() {
    let server = mailbox_of(1203);
    server.change_after(0, "Email", |store| {
        let mut seen = email(0);
        seen["keywords"] = serde_json::json!({ "$seen": true });
        store.upsert(seen);
    });
    let provider = provider_over(&server);

    let sync = provider.sync_email(&account(), None).await.unwrap();
    let delta = provider
        .sync_email(&account(), Some(&sync.next_cursor))
        .await
        .unwrap();

    let SyncUpdate::Delta { patched, .. } = &delta.update else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(patched.len(), 1);
}

/// Stalwart lists a message it has just stored last, and sorts it into place a moment later
/// without moving `queryState`. Stored before the walk began, it is in no delta, so only the
/// closing listing can find it, where it moved to.
#[tokio::test]
async fn mail_sorted_into_place_during_the_walk_is_found_by_the_closing_listing() {
    let server = Arc::new(BoundedServer::new(LIMIT).with_order_by("rank"));
    server.seed(
        "Email",
        (0..1203).map(|n| {
            let mut message = email(n);
            message["rank"] = Value::from(format!("r{n:05}"));
            message
        }),
    );
    server.change("Email", |store| {
        let mut late = with_id(email(0), "x99999");
        late["rank"] = Value::from("z");
        store.upsert(late);
    });
    server.change_after(0, "Email", |store| {
        store.resort("x99999", "rank", Value::from("a"));
    });

    let sync = provider_over(&server)
        .sync_email(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1204);
    assert_eq!(present.len(), 1204);
    assert!(keys(objects).contains("x99999"));
    assert_eq!(
        server.request_count(),
        5,
        "three pages, a listing, and the one missed message"
    );
}

/// In the server's own order an update can move an object from ahead of the walk to behind it.
/// The query moved, so the ids are listed again and the one the walk passed over is fetched.
#[tokio::test]
async fn an_object_that_moves_behind_the_walk_is_found_by_a_fresh_listing() {
    let server = Arc::new(BoundedServer::new(LIMIT).with_order_by("title"));
    server.seed(
        "CalendarEvent",
        (0..1203).map(|n| {
            let mut event = event(n);
            event["title"] = Value::from(format!("T{n:05}"));
            event
        }),
    );
    server.change_after(0, "CalendarEvent", |store| {
        let mut moved = event(1100);
        moved["title"] = Value::from("A");
        store.upsert(moved);
    });

    let sync = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
    assert_eq!(present.len(), 1203);
    assert!(keys(objects).contains("e01100"));
    assert_eq!(
        server.request_count(),
        5,
        "three pages, a listing, and the one missed object"
    );
}

/// A query that keeps moving under every listing has no complete answer to store, so the pass
/// fails as one to retry rather than storing part of the scope. The server caps its pages, so the
/// listing takes several requests; one answered in a single page is answered at once.
#[tokio::test]
async fn a_listing_that_never_holds_still_fails_the_pass_for_a_retry() {
    let server = Arc::new(BoundedServer::new(LIMIT).with_query_cap(LIMIT));
    server.seed("CalendarEvent", (0..1203).map(event));
    for request in 0..30 {
        server.change_after(request, "CalendarEvent", move |store| {
            store.upsert(event(5000 + request));
        });
    }

    let error = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap_err();

    assert_eq!(error.class(), FailureClass::Retryable, "{error}");
}

/// The ids a walk anchored on, from the end of a trail of `len` in id order, at each step back
/// tried before `stop`.
fn anchors_before(len: usize, stop: usize) -> Vec<String> {
    anchors(len)
        .take_while(|&back| back != stop)
        .map(|back| format!("x{:05}", len - 1 - back))
        .collect()
}

/// A server capping its pages at 100 when 500 were asked for, and a step back of 512 ids: the
/// walk re-reads five pages with nothing new, which it allows for in the server's page size,
/// not the one it asked for.
#[tokio::test]
async fn a_long_step_back_is_allowed_for_in_the_pages_the_server_serves() {
    let server = Arc::new(BoundedServer::new(LIMIT).with_query_cap(100));
    server.seed("Email", (0..1203).map(email));
    let destroyed = anchors_before(600, 512);
    server.change_after(5, "Email", move |store| {
        for id in &destroyed {
            store.destroy(id);
        }
    });

    let sync = provider_over(&server)
        .sync_email(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
}

/// Every id the walk tries as its anchor has been destroyed: it starts again from the top, and
/// hands on nothing twice.
#[tokio::test]
async fn a_walk_whose_every_anchor_is_gone_starts_again_from_the_top() {
    let server = mailbox_of(1203);
    let destroyed = anchors_before(500, usize::MAX);
    server.change_after(0, "Email", move |store| {
        for id in &destroyed {
            store.destroy(id);
        }
    });

    let sync = provider_over(&server)
        .sync_email(&account(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1203);
    assert_eq!(present.len(), 1203, "nothing handed on twice");
}

/// A server that sends no `queryState` gives the closing listing nothing to compare, so it is
/// taken as it is: the snapshot finishes rather than failing on every pass. The server caps its
/// pages, so the listing takes several requests and has states to compare.
#[tokio::test]
async fn a_server_without_query_state_still_finishes_a_snapshot() {
    let server = Arc::new(
        BoundedServer::new(LIMIT)
            .with_query_cap(LIMIT)
            .without_query_state(),
    );
    server.seed("CalendarEvent", (0..1203).map(event));

    let sync = provider_over(&server)
        .sync_events(&account(), None)
        .await
        .unwrap();

    assert!(sync.update.is_snapshot());
}

#[tokio::test]
async fn a_pass_from_a_cursor_is_a_delta_without_walking() {
    let server = mailbox_of(10);
    let since = SyncState::new(server.state("Email"));
    server.change("Email", |store| store.destroy("x00003"));

    let delta = provider_over(&server)
        .sync_email(&account(), Some(&since))
        .await
        .unwrap();

    assert!(!delta.update.is_snapshot());
    assert_eq!(server.request_count(), 1);
}

#[test]
fn a_destroyed_anchor_is_looked_for_further_back_each_time() {
    assert_eq!(anchors(1).collect::<Vec<_>>(), [0]);
    assert_eq!(anchors(4).collect::<Vec<_>>(), [0, 1, 2, 3]);
    assert_eq!(anchors(9).collect::<Vec<_>>(), [0, 1, 2, 3, 8]);
    assert_eq!(
        anchors(1000).collect::<Vec<_>>(),
        [0, 1, 2, 3, 8, 16, 32, 64, 128, 256, 512, 999]
    );
}
