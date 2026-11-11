//! The helpers `live_shared_calendar.rs` drives the shared mailbox's calendar with: the gate,
//! the providers bound to either mailbox, and finding, awaiting and cleaning up an event.
//!
//! A module of its own so the suite stays under the 500-line limit; no other suite uses it.

use std::time::Duration;

use engine_core::{
    calendar::{Calendar, Event, ParticipationStatus},
    ids::AccountId,
    sync::SyncUpdate,
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_provider::{CalendarWrites, EventDeletion, Provider};
use provider_graph::{CalendarWindow, GraphCalendarProvider, GraphClient, MailboxPrincipal};

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live-shared-calendar").unwrap()
}

pub(crate) fn gated(test: &str) -> Option<(String, String)> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(token), Some(shared)) = (var("GRAPH_ACCESS_TOKEN"), var("GRAPH_SHARED_MAILBOX"))
    else {
        eprintln!(
            "skipping {test}: set GRAPH_ACCESS_TOKEN and GRAPH_SHARED_MAILBOX (work account)"
        );
        return None;
    };
    Some((token, shared))
}

pub(crate) fn zone() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

/// A day far in the future, different per run, so neither a stray event from an earlier run
/// nor anything real shares it.
pub(crate) fn far_day() -> time::Date {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    time::Date::from_calendar_date(2027, time::Month::March, 1).unwrap()
        + time::Duration::days(i64::try_from(nanos % 200).unwrap())
}

pub(crate) fn at(day: time::Date, hour: u8) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: format!("{day}T{hour:02}:00:00")
            .parse::<LocalDateTime>()
            .unwrap(),
        zone: zone(),
    }
}

pub(crate) fn client(token: &str, principal: MailboxPrincipal) -> GraphClient {
    GraphClient::for_mailbox(
        token,
        principal,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

pub(crate) fn calendars_of(
    token: &str,
    principal: MailboxPrincipal,
    day: time::Date,
) -> GraphCalendarProvider {
    let window = CalendarWindow::new(
        (day - time::Duration::days(1)).to_string().parse().unwrap(),
        (day + time::Duration::days(1)).to_string().parse().unwrap(),
    );
    GraphCalendarProvider::new(client(token, principal), window, zone())
}

/// The calendar provider of the shared mailbox, bound the way a host binds it: through the
/// handle the calendar provider's own resolve handed out.
pub(crate) async fn shared_calendars(
    token: &str,
    shared: &str,
    day: time::Date,
) -> GraphCalendarProvider {
    let store = calendars_of(token, MailboxPrincipal::Me, day)
        .resolve_shared_account(shared)
        .await
        .expect("the shared mailbox's calendar resolves");
    calendars_of(token, MailboxPrincipal::shared(&store.handle).unwrap(), day)
}

pub(crate) async fn default_calendar(provider: &GraphCalendarProvider) -> Calendar {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync
        .update
    else {
        panic!("a snapshot");
    };
    objects
        .into_iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
}

/// The event titled `title`. By title, not UID: Graph assigns a created event a UID of its own,
/// so the one a draft names is not what reads back (as in every Graph live suite).
pub(crate) async fn find(
    provider: &GraphCalendarProvider,
    calendar: &Calendar,
    title: &str,
) -> Option<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(&account(), &calendar.id, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("a snapshot");
    };
    objects.into_iter().find(|e| e.title == title)
}

pub(crate) async fn await_event(
    provider: &GraphCalendarProvider,
    calendar: &Calendar,
    title: &str,
    what: &str,
) -> Event {
    for _ in 0..30 {
        if let Some(event) = find(provider, calendar, title).await {
            return event;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("timed out waiting for {what}");
}

pub(crate) async fn own_address(token: &str) -> String {
    let addresses = calendars_of(token, MailboxPrincipal::Me, far_day())
        .calendar_user_addresses(&account())
        .await
        .expect("the delegate's own calendar user");
    addresses.addresses()[0].clone()
}

/// A UID and a title no earlier run used; the title is what finds the event again.
pub(crate) fn unique(what: &str) -> (String, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    (
        format!("live-shared-calendar-{what}-{nanos}@allodia-e2e.test"),
        format!("PIM live shared-calendar {what} {nanos} (safe to delete)"),
    )
}

pub(crate) fn status_of(event: &Event, address: &str) -> Option<ParticipationStatus> {
    event
        .participants
        .iter()
        .find(|p| {
            p.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(address))
        })
        .map(|p| p.participation_status.clone())
}

pub(crate) async fn delete_titled(
    provider: &GraphCalendarProvider,
    calendar: &Calendar,
    title: &str,
) {
    if let Some(event) = find(provider, calendar, title).await {
        let _ = provider
            .delete_event(
                &account(),
                None,
                &EventDeletion::unconditional(event.id, event.uid),
            )
            .await;
    }
}
