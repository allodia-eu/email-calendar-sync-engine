use std::{num::NonZeroU64, sync::Arc};

use async_trait::async_trait;
use engine_core::{
    calendar::Calendar,
    error::FailureClass,
    ids::{AccountId, CalendarId, EventId},
    raw::RawIcal,
    sync::SyncState,
    version::{ETag, RevisionTokens},
};

use super::{CalendarImportLimits, CalendarImporter, EventImport, ImportedEvent};
use crate::{CalendarCollections, NewCalendar, ProviderResult};

fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

fn calendar() -> CalendarId {
    CalendarId::try_from("/cal/work/").unwrap()
}

fn event_of(bytes: usize) -> EventImport {
    EventImport::new(RawIcal::new("x".repeat(bytes)), calendar())
}

#[test]
fn an_event_never_prints_its_text() {
    let event = EventImport::new(RawIcal::new("SUMMARY:private review"), calendar());
    let printed = format!("{event:?}");
    assert!(!printed.contains("private"), "{printed}");
}

#[test]
fn a_receipt_says_whether_the_calendar_already_held_the_uid() {
    let key = EventId::try_from("/cal/work/a.ics").unwrap();
    let etag = RevisionTokens::from_etag(ETag::new("\"1\""));
    let created = ImportedEvent::created(key.clone(), etag.clone());
    let existing = ImportedEvent::existing(None);

    assert_eq!(created.key, Some(key));
    assert!(!created.already_there);
    assert_eq!(created.revisions, etag);
    assert_eq!(existing.key, None);
    assert!(existing.already_there);
    assert_eq!(existing.revisions, RevisionTokens::none());
}

#[test]
fn a_calendar_without_a_stated_limit_takes_any_size() {
    let limits = CalendarImportLimits::new();
    assert_eq!(limits.max_event_size(), None);
    assert!(limits.check(&event_of(10_000_000)).is_ok());
}

#[test]
fn an_event_over_the_stated_size_will_never_be_taken() {
    let limits = CalendarImportLimits::new().with_max_event_size(NonZeroU64::new(100).unwrap());

    assert!(limits.check(&event_of(100)).is_ok());
    let refused = limits.check(&event_of(101)).unwrap_err();
    assert!(refused.is_item_unavailable());
    assert_eq!(refused.class(), FailureClass::Permanent);
    assert_eq!(limits.max_event_size(), NonZeroU64::new(100));
}

/// An importer that answers fixed values, so delegation can be seen to reach it.
struct Fixed;

#[async_trait]
impl CalendarImporter for Fixed {
    async fn calendar_import_limits(
        &self,
        _account: &AccountId,
        _calendar: &CalendarId,
    ) -> ProviderResult<CalendarImportLimits> {
        Ok(CalendarImportLimits::new().with_max_event_size(NonZeroU64::new(7).unwrap()))
    }

    async fn import_cursor(
        &self,
        _account: &AccountId,
        _calendar: &CalendarId,
    ) -> ProviderResult<SyncState> {
        Ok(SyncState::new("c1"))
    }

    async fn import_events(
        &self,
        _account: &AccountId,
        batch: &[EventImport],
    ) -> Vec<ProviderResult<ImportedEvent>> {
        batch
            .iter()
            .map(|_| Ok(ImportedEvent::existing(None)))
            .collect()
    }
}

#[async_trait]
impl CalendarCollections for Fixed {
    async fn create_calendar(
        &self,
        _account: &AccountId,
        new: &NewCalendar,
    ) -> ProviderResult<Calendar> {
        let mut calendar = Calendar::new(calendar(), new.name.clone());
        calendar.color.clone_from(&new.color);
        calendar.description.clone_from(&new.description);
        Ok(calendar)
    }
}

async fn drive(importer: &dyn CalendarImporter) -> (Option<NonZeroU64>, SyncState, usize) {
    let limits = importer
        .calendar_import_limits(&account(), &calendar())
        .await
        .unwrap();
    let cursor = importer
        .import_cursor(&account(), &calendar())
        .await
        .unwrap();
    let answers = importer
        .import_events(&account(), &[event_of(1), event_of(2)])
        .await;
    (limits.max_event_size(), cursor, answers.len())
}

#[tokio::test]
async fn a_boxed_or_shared_importer_delegates_every_method() {
    let expected = (NonZeroU64::new(7), SyncState::new("c1"), 2);
    let boxed: Box<dyn CalendarImporter> = Box::new(Fixed);
    let shared: Arc<dyn CalendarImporter> = Arc::new(Fixed);

    assert_eq!(drive(&boxed).await, expected);
    assert_eq!(drive(&shared).await, expected);
}

#[tokio::test]
async fn a_boxed_or_shared_creator_delegates() {
    let new = NewCalendar::new("Work")
        .with_color("#FF8800")
        .with_description("Projects");
    let boxed: Box<dyn CalendarCollections> = Box::new(Fixed);
    let shared: Arc<dyn CalendarCollections> = Arc::new(Fixed);

    for creator in [&boxed as &dyn CalendarCollections, &shared] {
        let made = creator.create_calendar(&account(), &new).await.unwrap();
        assert_eq!(made.name, "Work");
        assert_eq!(made.color.as_deref(), Some("#FF8800"));
        assert_eq!(made.description.as_deref(), Some("Projects"));
    }
}

/// An adapter that imports events and creates calendars, reached only through `Provider`.
struct CalendarTarget {
    inner: crate::tests::FakeJmap,
}

#[async_trait]
impl crate::Provider for CalendarTarget {
    fn connection_info(&self) -> crate::ConnectionInfo {
        self.inner.connection_info()
    }

    fn calendar_importer(&self) -> Option<&dyn CalendarImporter> {
        Some(&Fixed)
    }

    fn calendar_collections(&self) -> Option<&dyn CalendarCollections> {
        Some(&Fixed)
    }
}

impl crate::CalendarSync for CalendarTarget {}
impl crate::MailboxWrites for CalendarTarget {}
impl crate::CalendarWrites for CalendarTarget {}

fn fake() -> crate::tests::FakeJmap {
    crate::tests::FakeJmap {
        info: crate::ConnectionInfo::new(crate::Capabilities::none().with_calendars()),
    }
}

// Through code generic over `P: Provider`, as a host's engine calls take it, which is the
// `Box<dyn Provider>` impl's delegation.
fn reach<P: crate::Provider + ?Sized>(
    provider: &P,
) -> (
    Option<&dyn CalendarImporter>,
    Option<&dyn CalendarCollections>,
) {
    (
        provider.calendar_importer(),
        provider.calendar_collections(),
    )
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_both_through_the_accessors() {
    let held: Box<dyn crate::Provider> = Box::new(CalendarTarget { inner: fake() });
    assert!(reach(&held).0.is_some() && reach(&held).1.is_some());
    assert!(reach(&Box::new(fake())).0.is_none());

    let importer = held.calendar_importer().expect("an importing adapter");
    assert_eq!(drive(importer).await.2, 2);
    let creator = held.calendar_collections().expect("a creating adapter");
    let made = creator
        .create_calendar(&account(), &NewCalendar::new("Home"))
        .await
        .unwrap();
    assert_eq!(made.name, "Home");
}

#[test]
fn a_provider_that_cannot_answers_none() {
    let plain: Box<dyn crate::Provider> = Box::new(fake());
    assert!(plain.calendar_importer().is_none());
    assert!(plain.calendar_collections().is_none());
}
