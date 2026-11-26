//! The calendar-sync trait's defaults: the JMAP scopes it names, and the
//! calendar verbs a provider that serves no calendar rejects. A sibling of `tests.rs` so that file
//! stays under the line limit.

use engine_core::{
    sync::{JmapDataType, SyncScope},
    time::Horizon,
};

use crate::{
    CalendarSync, Capabilities, ConnectionInfo,
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

/// An adapter that names a new event's resource itself, as a document transport does.
struct Mints;

impl crate::CalendarWrites for Mints {
    fn new_event_id(
        &self,
        calendar: &engine_core::ids::CalendarId,
        uid: &engine_core::ids::Uid,
    ) -> crate::ProviderResult<engine_core::ids::EventId> {
        Ok(engine_core::ids::EventId::try_from(
            format!("{}{}.ics", calendar.as_str(), uid.as_str()).as_str(),
        )
        .unwrap())
    }
}

#[test]
fn a_new_event_id_defaults_to_unsupported_held_directly_or_boxed() {
    use crate::CalendarWrites;

    let uid = engine_core::ids::Uid::new("e-1").unwrap();
    let direct = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };
    let boxed: Box<dyn crate::Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    for err in [
        direct.new_event_id(&calendar(), &uid).unwrap_err(),
        boxed.new_event_id(&calendar(), &uid).unwrap_err(),
    ] {
        assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
    }
}

/// The rejecting default above cannot tell a forwarding `Box` from one that forgot to
/// forward, since both answer unsupported; an adapter that mints can.
#[test]
fn a_box_asks_the_adapter_it_holds_for_a_new_event_id() {
    use crate::CalendarWrites;

    let boxed: Box<dyn CalendarWrites> = Box::new(Mints);
    let minted = boxed
        .new_event_id(&calendar(), &engine_core::ids::Uid::new("e-1").unwrap())
        .unwrap();
    assert_eq!(minted.as_str(), "/cal/e-1.ics");
}
