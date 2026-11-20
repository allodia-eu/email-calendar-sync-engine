//! The calendar half of the provider trait's defaults: the JMAP scopes it names, and the
//! calendar verbs a provider that serves no calendar rejects. A sibling of `tests.rs` so that file
//! stays under the line limit.

use engine_core::{
    sync::{JmapDataType, SyncScope},
    time::Horizon,
};

use crate::{
    Capabilities, ConnectionInfo, Provider,
    tests::{FakeJmap, account, calendar},
};

/// A window wide enough to be the whole calendar.
pub(crate) fn whole() -> Horizon {
    Horizon::new(
        "2000-01-01T00:00:00Z".parse().unwrap(),
        "2100-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn calendar_methods_default_to_unsupported_with_jmap_scopes() {
    let provider = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };
    assert_eq!(
        provider.calendar_scope(&account()),
        SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Calendar,
        }
    );
    assert_eq!(
        provider.event_scope(&account(), &calendar()),
        SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::CalendarEvent,
        }
    );
    assert!(provider.sync_calendars(&account(), None).await.is_err());
    assert!(
        provider
            .sync_events(&account(), &calendar(), None, whole())
            .await
            .is_err()
    );
}
