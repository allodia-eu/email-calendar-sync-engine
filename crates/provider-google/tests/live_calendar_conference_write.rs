//! Gated live check that the adapter puts a video meeting on an event and takes it off again,
//! both ways a meeting reaches a Google event: Meet, which Google makes, and a link made
//! elsewhere, which Google stores as an add-on conference. Every step goes through the
//! adapter's own writes and is read back through its own sync, which is what a host sees.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_calendar_conference_write -- --nocapture
//! ```

mod common;

use common::*;
use engine_core::{
    calendar::{
        Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceText,
        ConferenceUri, EntryPoint, Event,
    },
    error::FailureClass,
    ids::{CalendarId, Uid},
    sync::SyncUpdate,
};
use engine_provider::{
    CalendarWrites, ConferenceEdit, EventDeletion, EventEdit, EventPatch, NewConference,
    PatchTarget, Provider,
};
use provider_google::GoogleCalendarProvider;

const JITSI: &str = "https://meet.example.org/pim-live-room-4321";

fn words() -> ConferenceText {
    ConferenceText {
        meeting: "Video meeting".into(),
        join: "Join".into(),
        meeting_id: "Meeting ID".into(),
        passcode: "Passcode".into(),
        dial_in: "Dial in".into(),
    }
}

fn jitsi() -> NewConference {
    NewConference::attach(
        Conference {
            key: ConferenceKey::new(JITSI),
            service: Some(ConferenceService::Jitsi),
            origin: ConferenceOrigin::Attached,
            entry_points: vec![EntryPoint::at(ConferenceUri::parse(JITSI).unwrap())],
            label: Some("Jitsi Meet".into()),
            meeting_id: None,
            notes: None,
        },
        words(),
    )
}

#[tokio::test]
async fn live_a_meeting_goes_on_and_comes_off_both_ways() {
    let Some(token) = token() else {
        eprintln!("skipping live_a_meeting_goes_on_and_comes_off...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(17);
    let provider = calendar_provider(token);
    let primary = primary(&provider).await;
    let calendars = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync;
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let calendar = objects
        .iter()
        .find(|c| c.id == primary)
        .expect("the primary calendar");
    if !calendar
        .native_conferences
        .contains(&ConferenceService::GoogleMeet)
    {
        eprintln!("skipping: this calendar makes no Meet meetings");
        return;
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uid = Uid::new(format!("live-conference-{unique}@allodia-e2e.test")).unwrap();
    let title = format!("PIM live meeting writes {unique}");
    let draft = engine_provider::EventDraft::new(
        calendar.id.clone(),
        uid,
        &title,
        zoned(&format!("{date}T10:00:00")),
        zoned(&format!("{date}T10:30:00")),
        "2026-07-18T10:00:00Z".parse().unwrap(),
    )
    .description("Bring the figures.")
    .conference(NewConference::Native(ConferenceService::GoogleMeet));
    provider
        .create_event(&account(), &draft)
        .await
        .expect("create with Meet");

    let outcome = steps(&provider, &calendar.id, &title).await;
    if let Some(event) = read(&provider, &calendar.id, &title).await {
        provider
            .delete_event(
                &account(),
                None,
                &EventDeletion::unconditional(event.id, event.uid),
            )
            .await
            .expect("delete the fixture event");
    }
    outcome.unwrap();
}

/// Removes Meet, attaches a link made elsewhere, removes that, checking each read-back.
async fn steps(
    provider: &GoogleCalendarProvider,
    calendar: &CalendarId,
    title: &str,
) -> Result<(), String> {
    let event = read(provider, calendar, title)
        .await
        .ok_or("the created event syncs")?;
    let meet = match event.conferences.as_slice() {
        [conference] if conference.service == Some(ConferenceService::GoogleMeet) => {
            conference.clone()
        }
        other => return Err(format!("one Meet meeting, got {other:?}")),
    };
    if !matches!(meet.origin, ConferenceOrigin::Native { .. }) {
        return Err(format!("Meet is the calendar's own: {meet:?}"));
    }

    edit(
        provider,
        calendar,
        title,
        ConferenceEdit::Remove(meet.key.clone()),
    )
    .await?;
    let event = read(provider, calendar, title)
        .await
        .ok_or("syncs after removal")?;
    if !event.conferences.is_empty() {
        return Err(format!("Meet came off: {:?}", event.conferences));
    }
    if event.description.as_deref().map(str::trim) != Some("Bring the figures.") {
        return Err(format!(
            "the notes are the person's: {:?}",
            event.description
        ));
    }

    edit(
        provider,
        calendar,
        title,
        ConferenceEdit::Set {
            replacing: None,
            new: jitsi(),
        },
    )
    .await?;
    let event = read(provider, calendar, title)
        .await
        .ok_or("syncs after attach")?;
    let attached = match event.conferences.as_slice() {
        [conference]
            if conference.join().map(ConferenceUri::as_str) == Some(JITSI)
                && conference.origin == ConferenceOrigin::Attached =>
        {
            conference.clone()
        }
        other => {
            return Err(format!(
                "one add-on meeting at the attached link, got {other:?}"
            ));
        }
    };
    if event.description.as_deref() != Some("Bring the figures.") {
        return Err(format!(
            "Google writes no block of ours: {:?}",
            event.description
        ));
    }

    edit(
        provider,
        calendar,
        title,
        ConferenceEdit::Remove(attached.key.clone()),
    )
    .await?;
    let event = read(provider, calendar, title)
        .await
        .ok_or("syncs after the last removal")?;
    if !event.conferences.is_empty() || !event.locations.is_empty() {
        return Err(format!(
            "the link left the notes and the location: {:?} {:?}",
            event.conferences, event.locations
        ));
    }
    Ok(())
}

/// Applies `conference` to the event as it now reads. Google finishes a Meet request after
/// the write, so a revision read a moment earlier can be refused as stale (`412`); like the
/// outbox, the edit is then rebuilt on a fresh read.
async fn edit(
    provider: &GoogleCalendarProvider,
    calendar: &CalendarId,
    title: &str,
    conference: ConferenceEdit,
) -> Result<(), String> {
    for _ in 0..5 {
        let base = read(provider, calendar, title)
            .await
            .ok_or("the event syncs")?;
        let edit = EventEdit::new(
            &base,
            PatchTarget::Series,
            EventPatch::new("2026-07-18T10:05:00Z".parse().unwrap()).conference(conference.clone()),
        );
        match provider.patch_event(&account(), &base, &edit).await {
            Ok(_) => return Ok(()),
            Err(err) if err.class() == FailureClass::Conflict => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(err) => return Err(format!("patch_event: {err}")),
        }
    }
    Err("the edit kept conflicting".to_owned())
}

async fn read(
    provider: &GoogleCalendarProvider,
    calendar: &CalendarId,
    title: &str,
) -> Option<Event> {
    let events = provider
        .sync_events(&account(), calendar, None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
        panic!("expected an event snapshot");
    };
    objects.into_iter().find(|event| event.title == title)
}
