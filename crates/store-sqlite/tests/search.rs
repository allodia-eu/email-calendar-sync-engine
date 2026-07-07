//! End-to-end search: project domain objects, store them with their derived rows,
//! then run mail/calendar queries through the SQLite executor and assert the
//! ranked answers and coverage.
//!
//! The cases live in the `search/` submodules declared below; this binary holds
//! the shared fixtures/helpers they reach via `super::`.

use core::time::Duration;

use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{EmailAddress, Message},
    membership::Memberships,
    search_index::project_message,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_search::MailQuery;
use engine_store::{ApplyBatch, DerivedWrite, LeaseRequest, ManualClock, Store, WorkerId};
use store_sqlite::SqliteStore;

#[path = "search/body.rs"]
mod body;
#[path = "search/calendar.rs"]
mod calendar;
#[path = "search/mail.rs"]
mod mail;

fn store() -> SqliteStore<ManualClock> {
    SqliteStore::open_in_memory(ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap()))
        .expect("open")
}

fn account() -> engine_core::ids::AccountId {
    engine_core::ids::AccountId::try_from("acct-1").unwrap()
}

fn mail_scope() -> SyncScope {
    SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::Email,
    }
}

fn calendar_scope() -> SyncScope {
    SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::CalendarEvent,
    }
}

fn lease() -> LeaseRequest {
    // The manual clock never advances in these tests, so any positive TTL keeps
    // the lease live for the whole ingest.
    LeaseRequest::new(WorkerId::new("w"), Duration::from_secs(30))
}

/// Builds a message in `mailbox` with a subject and a single `from` address.
fn message(id: &str, subject: &str, from: &str, mailbox: &str) -> Message {
    let mut m = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    m.envelope.subject = Some(subject.to_owned());
    m.envelope.from = vec![EmailAddress::new(from)];
    m
}

async fn ingest_mail(store: &SqliteStore<ManualClock>, scope: &SyncScope, messages: Vec<Message>) {
    let claim = store
        .claim_sync_scope(account(), scope, lease())
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    for m in &messages {
        derived.push_mail(project_message(m));
    }
    let update = SyncUpdate::delta(messages, vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("c1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();
}

fn parse_mail(query: &str) -> MailQuery {
    MailQuery::parse(query).unwrap()
}

fn pk(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

fn zoned(year: i32, month: u8, day: u8, hour: u8) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(year, month, day, hour, 0, 0).unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}
