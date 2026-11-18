//! A reach that waited for the scope's lease builds on what the pass holding it recorded:
//! every window it decides on is read under its own lease, never before the claim.
//!
//! Each test lets the other pass run to completion in the gap between the reach starting and
//! its claim ([`BeforeClaim`]), which is where a read made before the claim goes stale.

use super::*;
use crate::{CalendarReach, EventScopePass, tests::before_claim::BeforeClaim};

/// The pass that holds the scope first, under a worker of its own.
fn first() -> WorkerId {
    WorkerId::new("first")
}

/// `store`, with `hook` run before this test's worker first claims the event scope.
fn racing(
    store: &Arc<SqliteStore<ManualClock>>,
    hook: impl Future<Output = ()> + Send + 'static,
) -> BeforeClaim<SqliteStore<ManualClock>> {
    BeforeClaim {
        inner: Arc::clone(store),
        worker: worker(),
        hook: Mutex::new(Some(Box::pin(hook))),
    }
}

async fn reach_through<S: Store + StoreRead>(
    provider: &FakeWindowed,
    store: &S,
    worker: WorkerId,
    range: Horizon,
) -> CalendarReach {
    let reach = reach_calendar(
        provider,
        store,
        &account(),
        worker,
        Duration::from_mins(1),
        range,
    )
    .await
    .unwrap();
    assert!(reach.is_complete());
    reach
}

fn february() -> Horizon {
    horizon("2026-02-01T00:00:00Z", "2026-03-01T00:00:00Z")
}

fn june() -> Horizon {
    horizon("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z")
}

#[tokio::test]
async fn a_reach_after_another_keeps_the_range_that_one_fetched() {
    let provider = Arc::new(FakeWindowed::windowed());
    provider.put("evt-feb", 2, 10);
    provider.put("evt-march", 3, 10);
    provider.put("evt-june", 6, 10);
    let store = Arc::new(SqliteStore::open_in_memory(clock()).unwrap());
    sync(&provider, &store).await;

    // February is reached between this call starting and its claim.
    let (other, inner) = (Arc::clone(&provider), Arc::clone(&store));
    let store = racing(&store, async move {
        reach_through(&other, &*inner, first(), february()).await;
    });
    let reach = reach_through(&provider, &store, worker(), june()).await;
    assert!(matches!(reach.scopes[0].result, Ok(ScopeReach::Fetched(_))));

    let whole = horizon("2026-02-01T00:00:00Z", "2026-07-01T00:00:00Z");
    assert_eq!(
        store.fetch_window(&scope()).await.unwrap(),
        Some(whole),
        "the window only grows"
    );
    assert_eq!(
        stored(&store.inner).await,
        vec![key("evt-feb"), key("evt-june"), key("evt-march")],
        "February, which the other reach fetched, is still held"
    );
}

#[tokio::test]
async fn a_reach_after_an_expansion_expands_what_it_fetches_over_the_wider_window() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-june", 6, 10);
    let store = Arc::new(SqliteStore::open_in_memory(clock()).unwrap());
    sync(&provider, &store).await;

    // The host widens what it shows to June between this call starting and its claim.
    let shown_to_june = horizon("2026-03-01T00:00:00Z", "2026-07-01T00:00:00Z");
    let inner = Arc::clone(&store);
    let store = racing(&store, async move {
        expand_calendar_horizon(
            &*inner,
            &account(),
            first(),
            Duration::from_mins(1),
            shown_to_june,
            &zone(),
        )
        .await
        .unwrap();
    });
    reach_through(&provider, &store, worker(), june()).await;

    assert_eq!(
        store
            .scope_occurrences(&scope(), june())
            .await
            .unwrap()
            .len(),
        1,
        "the reached event is on the grid the host now shows"
    );
}

#[tokio::test]
async fn a_reach_keeps_the_tag_that_lets_an_unchanged_calendar_be_skipped() {
    let provider = FakeWindowed {
        tag: Some(CollectionTag::new("v1")),
        ..FakeWindowed::windowed()
    };
    provider.put("evt-june", 6, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    reach_through(&provider, &store, worker(), june()).await;
    provider.take_requests();

    let report = sync_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        shown(),
        &zone(),
    )
    .await
    .unwrap();
    assert!(matches!(
        report.scopes[0].result,
        Ok(EventScopePass::Unchanged)
    ));
    assert_eq!(provider.take_requests(), []);
}
