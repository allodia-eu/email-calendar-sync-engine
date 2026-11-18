//! An expansion that finds one of an account's calendars held waits for it, but not before it
//! has widened every calendar nobody holds.

use engine_core::{ids::DavCollectionId, time::ExpansionWindow};

use super::*;

const TTL: Duration = Duration::from_mins(1);

fn scope(path: &str) -> SyncScope {
    SyncScope::DavCollection {
        account: account(),
        collection: DavCollectionId::new(ProviderKey::new(path).unwrap()),
    }
}

fn window(from: &str, to: &str) -> ExpansionWindow {
    let horizon = Horizon::new(from.parse().unwrap(), to.parse().unwrap()).unwrap();
    ExpansionWindow::new(horizon, TimeZoneId::iana("Europe/Amsterdam").unwrap())
}

async fn expansion(store: &SqliteStore<ManualClock>, scope: &SyncScope) -> ExpansionWindow {
    store.expansion_window(scope).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_held_calendar_does_not_keep_the_others_narrow() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let narrow = window("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z");
    for path in ["/cal/home/", "/cal/work/"] {
        let claim = store
            .claim_sync_scope(account(), &scope(path), LeaseRequest::new(worker(), TTL))
            .await
            .unwrap();
        store
            .set_expansion_window(&claim.lease, &narrow)
            .await
            .unwrap();
        store.release_sync_scope(claim.lease).await.unwrap();
    }
    // Hold whichever scope the expansion reaches first, so the other one comes after it.
    let scopes = store.account_scopes(account()).await.unwrap();
    let [first, second] = <[_; 2]>::try_from(scopes).unwrap();
    let held = store
        .claim_sync_scope(
            account(),
            &first,
            LeaseRequest::new(WorkerId::new("other"), TTL),
        )
        .await
        .unwrap();

    let wider = window("2026-02-01T00:00:00Z", "2026-05-01T00:00:00Z");
    let (account, zone) = (account(), wider.zone.clone());
    let expanding = expand_calendar_horizon(&store, &account, worker(), TTL, wider.horizon, &zone);
    tokio::pin!(expanding);
    tokio::select! {
        _ = &mut expanding => panic!("expanded a scope another pass holds"),
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    assert_eq!(
        expansion(&store, &second).await,
        wider,
        "the free one is not kept waiting"
    );
    assert_eq!(expansion(&store, &first).await, narrow);

    store.release_sync_scope(held.lease).await.unwrap();
    expanding.await.unwrap();
    assert_eq!(expansion(&store, &first).await, wider);
}
