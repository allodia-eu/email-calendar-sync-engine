//! Gated live check that the adapter puts a meeting on a JSCalendar event and takes it off
//! again, against the Stalwart harness. Each step is read back through the adapter's own sync
//! and asserts both what must be there and what must be gone, so an adapter that wrote nothing
//! cannot pass. Skips with no `STALWART_HTTP_ADDR`.

mod common;

use common::*;
use engine_core::{
    calendar::{
        Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceText,
        ConferenceUri, EntryPoint, Event,
    },
    ids::Uid,
};
use engine_provider::{
    CalendarWrites, ConferenceEdit, EventDeletion, EventDraft, EventEdit, EventPatch,
    NewConference, PatchTarget,
};
use provider_jmap::JmapProvider;

const UID: &str = "jmap-meeting-write@test.local";
const TALK: &str = "https://cloud.example.com/call/live4711";
const DIAL: &str = "tel:+31201234567,,4711#";
const JITSI: &str = "https://meet.example.org/live-room-4711";
const NOTES: &str = "Bring the figures.";

fn meeting(service: ConferenceService, label: &str, uris: &[&str]) -> NewConference {
    NewConference::attach(
        Conference {
            key: ConferenceKey::new(uris[0]),
            service: Some(service),
            origin: ConferenceOrigin::Attached,
            entry_points: uris
                .iter()
                .map(|uri| EntryPoint::at(ConferenceUri::parse(uri).unwrap()))
                .collect(),
            label: Some(label.to_owned()),
            meeting_id: None,
            notes: None,
        },
        ConferenceText {
            meeting: "Video meeting".into(),
            join: "Join".into(),
            meeting_id: "Meeting ID".into(),
            passcode: "Passcode".into(),
            dial_in: "Dial in".into(),
            dial_in_code: "Conference ID".into(),
        },
    )
}

#[tokio::test]
async fn a_meeting_goes_on_and_comes_off() {
    let Some(provider) = setup("a_meeting_goes_on_and_comes_off").await else {
        return;
    };
    pre_clean(&provider, UID).await;
    let draft = EventDraft::new(
        calendar(&provider).await,
        Uid::new(UID).unwrap(),
        "Live meeting writes",
        amsterdam("2026-09-14T09:00:00"),
        amsterdam("2026-09-14T09:30:00"),
        stamp(),
    )
    .description(NOTES)
    .conference(meeting(
        ConferenceService::NextcloudTalk,
        "Nextcloud Talk",
        &[TALK, DIAL],
    ));
    provider
        .create_event(&account(), &draft)
        .await
        .expect("create with a meeting");

    let outcome = steps(&provider).await;
    if let Some(event) = fetch(&provider, UID).await {
        provider
            .delete_event(&account(), None, &EventDeletion::of(&event))
            .await
            .expect("delete the fixture event");
    }
    outcome.unwrap();
}

async fn steps(provider: &JmapProvider) -> Result<(), String> {
    let event = read(provider).await?;
    let talk = match event.conferences.as_slice() {
        [one] if one.join().map(ConferenceUri::as_str) == Some(TALK) => one.clone(),
        other => return Err(format!("one Talk meeting, got {other:?}")),
    };
    let ways_in: Vec<&str> = talk.entry_points.iter().map(|e| e.uri.as_str()).collect();
    check(
        ways_in == [TALK, DIAL],
        "the dial-in joins the meeting",
        &ways_in,
    )?;
    check(
        talk.origin == ConferenceOrigin::Attached
            && talk.label.as_deref() == Some("Nextcloud Talk"),
        "the meeting is stored in virtualLocations under its name",
        &talk,
    )?;
    check(
        location(&event) == Some(TALK),
        "the link is the location",
        &event.locations,
    )?;
    check(
        notes(&event) == Some(NOTES.to_owned()),
        "the notes are the person's",
        &event.description,
    )?;

    edit(
        provider,
        &event,
        ConferenceEdit::Set {
            replacing: Some(talk.key.clone()),
            new: meeting(ConferenceService::Jitsi, "Jitsi Meet", &[JITSI]),
        },
    )
    .await?;
    let event = read(provider).await?;
    let jitsi = match event.conferences.as_slice() {
        [one] if one.join().map(ConferenceUri::as_str) == Some(JITSI) => one.clone(),
        other => return Err(format!("only the Jitsi meeting, got {other:?}")),
    };
    let raw = event
        .raw_jscalendar
        .as_ref()
        .map(|raw| raw.as_str().to_owned());
    check(
        raw.as_deref().is_some_and(|raw| !raw.contains("live4711")),
        "nothing of the Talk meeting is left",
        &raw,
    )?;
    check(
        location(&event) == Some(JITSI),
        "the new link is the location",
        &event.locations,
    )?;
    check(
        notes(&event) == Some(NOTES.to_owned()),
        "the notes are unchanged",
        &event.description,
    )?;

    edit(provider, &event, ConferenceEdit::Remove(jitsi.key)).await?;
    let event = read(provider).await?;
    check(
        event.conferences.is_empty(),
        "the meeting came off",
        &event.conferences,
    )?;
    check(
        event.locations.is_empty(),
        "its location went with it",
        &event.locations,
    )?;
    check(
        event.description.as_deref().map(str::trim) == Some(NOTES),
        "the block went with it",
        &event.description,
    )
}

async fn edit(
    provider: &JmapProvider,
    base: &Event,
    conference: ConferenceEdit,
) -> Result<(), String> {
    let edit = EventEdit::new(
        base,
        PatchTarget::Series,
        EventPatch::new(stamp()).conference(conference),
    );
    provider
        .patch_event(&account(), base, &edit)
        .await
        .map(drop)
        .map_err(|err| format!("patch_event: {err}"))
}

async fn read(provider: &JmapProvider) -> Result<Event, String> {
    fetch(provider, UID)
        .await
        .ok_or_else(|| "the event syncs".to_owned())
}

fn check(ok: bool, what: &str, seen: &impl std::fmt::Debug) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("{what}: {seen:?}"))
    }
}

fn location(event: &Event) -> Option<&str> {
    event.locations.first().and_then(|l| l.name.as_deref())
}

fn notes(event: &Event) -> Option<String> {
    event.notes(|_| true).map(std::borrow::Cow::into_owned)
}
