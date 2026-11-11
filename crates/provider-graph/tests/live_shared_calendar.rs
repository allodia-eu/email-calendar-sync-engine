//! Gated live checks: the calendar of a shared mailbox, opened under `/users/{address}` by a
//! delegate with Full Access, against a real Microsoft 365 tenant.
//!
//! Needs a **work/school** token and the address of a mailbox shared with it. Neither is ever
//! written here — the mailbox is somebody's real one:
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml \
//!     -- --profile m365 token)" \
//! GRAPH_SHARED_MAILBOX="<a mailbox shared with that account>" \
//!   cargo test -p provider-graph --test live_shared_calendar -- --nocapture --test-threads=1
//! ```
//!
//! **Writes**, unlike `live_shared.rs`: events in the shared mailbox's calendar and in the
//! delegate's own, and meetings between the two — never anyone else, so nothing leaves the
//! tenant. Each test deletes what it made, dates far in the future and titled so a stray one is
//! recognisable. Run one at a time: two meetings between the same two mailboxes cross in each
//! other's calendars.
//!
//! Not covered, because no such share exists on the test tenant yet: a delegate granted the
//! calendar alone, or granted it read-only.

mod shared_calendar;

use std::time::Duration;

use engine_core::{
    calendar::{ConferenceOrigin, ConferenceService, ParticipantRole, ParticipationStatus},
    error::FailureClass,
    ids::Uid,
    sync::SearchDomain,
};
use engine_provider::{
    CalendarAddress, CalendarWrites, EventDraft, EventEdit, EventPatch, EventRsvp, Invitee,
    MeetingDraft, MeetingRequest, MeetingService, NewConference, PatchTarget, Provider,
    RsvpResponse, SchedulingIdentity, SharedAccounts,
};
use provider_graph::{MailboxPrincipal, TeamsMeetings};
use shared_calendar::{
    account, at, await_event, calendars_of, client, default_calendar, delete_titled, far_day, find,
    gated, own_address, shared_calendars, status_of, unique,
};

#[tokio::test]
async fn live_the_shared_mailboxs_calendar_resolves_and_the_own_one_never_does() {
    let Some((token, shared)) =
        gated("live_the_shared_mailboxs_calendar_resolves_and_the_own_one_never_does")
    else {
        return;
    };
    let own = calendars_of(&token, MailboxPrincipal::Me, far_day());
    assert_eq!(
        own.connection_info().capabilities.shared_accounts(),
        SharedAccounts::ByAddress
    );
    assert_eq!(
        own.list_shared_accounts().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    let resolved = own.resolve_shared_account(&shared).await.unwrap();
    assert_eq!(resolved.handle.as_str(), shared);
    assert_eq!(
        resolved.domains.iter().collect::<Vec<_>>(),
        [SearchDomain::Calendar]
    );

    let me = own_address(&token).await;
    for own_address in [me.clone(), me.to_uppercase()] {
        let err = own.resolve_shared_account(&own_address).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent, "{err}");
        assert!(err.detail().contains("own calendar"), "{err}");
    }
    let domain = shared.rsplit_once('@').map(|(_, d)| d).unwrap();
    let err = own
        .resolve_shared_account(&format!("no-such-mailbox-e2e-probe@{domain}"))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

/// The bound provider reads the shared mailbox's own calendars, not the delegate's: its
/// default calendar is a different calendar, owned by the shared mailbox, and its calendar user
/// is the shared mailbox (`engine_provider::calendar_user`).
#[tokio::test]
async fn live_a_bound_provider_reads_the_shared_mailboxs_calendar_as_its_owner() {
    let Some((token, shared)) =
        gated("live_a_bound_provider_reads_the_shared_mailboxs_calendar_as_its_owner")
    else {
        return;
    };
    let day = far_day();
    let bound = shared_calendars(&token, &shared, day).await;
    let theirs = default_calendar(&bound).await;
    let mine = default_calendar(&calendars_of(&token, MailboxPrincipal::Me, day)).await;
    assert_ne!(theirs.id, mine.id);
    assert!(
        theirs
            .owner
            .as_deref()
            .is_some_and(|o| o.eq_ignore_ascii_case(&shared)),
        "{theirs:?}"
    );
    assert!(theirs.access.may_write, "Full Access: {theirs:?}");

    let me = own_address(&token).await;
    let answer = bound.calendar_user_addresses(&account()).await.unwrap();
    assert!(answer.is_for_store(&shared, &me), "{answer:?}");
    assert!(answer.recognises(&shared), "{answer:?}");

    // `calendarView/delta` under the shared principal ends in a cursor.
    let sync = bound
        .sync_events(&account(), &theirs.id, None)
        .await
        .expect("delta sync under /users/{shared}");
    assert_ne!(sync.next_cursor.as_str(), "", "a delta cursor");
}

/// Create, guarded patch and delete all work under `/users/{shared}` as under `/me`, the stale
/// `If-Match` included.
#[tokio::test]
async fn live_writes_in_the_shared_calendar_are_guarded() {
    let Some((token, shared)) = gated("live_writes_in_the_shared_calendar_are_guarded") else {
        return;
    };
    let day = far_day();
    let bound = shared_calendars(&token, &shared, day).await;
    let calendar = default_calendar(&bound).await;
    let (uid, title) = unique("write");
    let draft = EventDraft::new(
        calendar.id.clone(),
        Uid::new(&uid).unwrap(),
        &title,
        at(day, 10),
        at(day, 11),
        "2026-11-11T10:00:00Z".parse().unwrap(),
    );
    bound
        .create_event(&account(), &draft)
        .await
        .expect("create in the shared calendar");
    let created = await_event(&bound, &calendar, &title, "the created event").await;

    let outcome = async {
        // Exchange touches a new item after creating it (indexing), so a patch guarded by the
        // first read can meet a `412` that no other writer caused; the fresh patch re-reads and
        // tries again, as `live_calendar_conference_write.rs` does.
        let mut patched = false;
        for _ in 0..5 {
            let base = find(&bound, &calendar, &title)
                .await
                .ok_or("the event syncs")?;
            let edited = EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new("2026-11-11T10:05:00Z".parse().unwrap()).description("Edited"),
            );
            match bound.patch_event(&account(), &base, &edited).await {
                Ok(_) => {
                    patched = true;
                    break;
                }
                Err(err) if err.class() == FailureClass::Conflict => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(err) => return Err(format!("fresh patch: {err}")),
            }
        }
        if !patched {
            return Err("a fresh patch kept meeting a conflict".to_owned());
        }
        // `created` now holds a superseded ETag.
        let stale = EventEdit::new(
            &created,
            PatchTarget::Series,
            EventPatch::new("2026-11-11T10:06:00Z".parse().unwrap()).description("Stale"),
        );
        match bound.patch_event(&account(), &created, &stale).await {
            Err(err) if err.class() == FailureClass::Conflict => Ok(()),
            other => Err(format!("a stale If-Match is a conflict, got {other:?}")),
        }
    }
    .await;
    delete_titled(&bound, &calendar, &title).await;
    outcome.unwrap();
    assert!(find(&bound, &calendar, &title).await.is_none(), "deleted");
}

/// A meeting made in the shared calendar is organised by the shared mailbox, and a Teams
/// meeting on it is the calendar's own. The delegate is the one invitee, so nothing leaves the
/// tenant.
#[tokio::test]
async fn live_a_meeting_from_the_shared_calendar_is_organised_by_the_shared_mailbox() {
    let Some((token, shared)) =
        gated("live_a_meeting_from_the_shared_calendar_is_organised_by_the_shared_mailbox")
    else {
        return;
    };
    let day = far_day();
    let me = own_address(&token).await;
    let bound = shared_calendars(&token, &shared, day).await;
    let mine = calendars_of(&token, MailboxPrincipal::Me, day);
    let theirs = default_calendar(&bound).await;
    let my_calendar = default_calendar(&mine).await;
    let (uid, title) = unique("meeting");
    let draft = EventDraft::new(
        theirs.id.clone(),
        Uid::new(&uid).unwrap(),
        &title,
        at(day, 14),
        at(day, 15),
        "2026-11-11T10:00:00Z".parse().unwrap(),
    )
    .conference(NewConference::Native(ConferenceService::Teams))
    .meeting(MeetingDraft::new(
        SchedulingIdentity::named(CalendarAddress::parse(&shared).unwrap(), "Shared"),
        vec![Invitee::required(SchedulingIdentity::new(
            CalendarAddress::parse(&me).unwrap(),
        ))],
    ));
    bound
        .create_event(&account(), &draft)
        .await
        .expect("create the meeting");

    let outcome = async {
        let organised = await_event(&bound, &theirs, &title, "the organiser's copy").await;
        let owner = organised
            .participants
            .iter()
            .find(|p| p.has_role(&ParticipantRole::Owner))
            .ok_or(format!("an organiser: {:?}", organised.participants))?;
        if !owner
            .email
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case(&shared))
        {
            return Err(format!("organised by the shared mailbox: {owner:?}"));
        }
        match organised.conferences.as_slice() {
            [teams]
                if teams.service == Some(ConferenceService::Teams)
                    && matches!(teams.origin, ConferenceOrigin::Native { .. }) => {}
            other => return Err(format!("one native Teams meeting: {other:?}")),
        }
        // The invitation reaches the delegate, and Exchange has already answered it: the
        // delegate's own copy reads accepted before anyone did (seen on 2026-11-10 and every
        // run since). Exchange treats the delegate as the meeting's author. Pinned so a change
        // in that is noticed; the engine relies on neither answer.
        let delivered = await_event(&mine, &my_calendar, &title, "the delegate's copy").await;
        if status_of(&delivered, &me) != Some(ParticipationStatus::Accepted) {
            return Err(format!(
                "the delegate's copy was expected to read accepted unasked: {:?}",
                delivered.participants
            ));
        }
        Ok::<(), String>(())
    }
    .await;
    delete_titled(&bound, &theirs, &title).await;
    delete_titled(&mine, &my_calendar, &title).await;
    outcome.unwrap();
}

/// The shared mailbox answers an invitation the delegate sent it, through the bound provider's
/// RSVP (`POST /users/{shared}/events/{id}/accept`), and the organiser's copy hears it.
#[tokio::test]
async fn live_the_shared_mailbox_accepts_an_invitation_on_its_own_behalf() {
    let Some((token, shared)) =
        gated("live_the_shared_mailbox_accepts_an_invitation_on_its_own_behalf")
    else {
        return;
    };
    let day = far_day();
    let me = own_address(&token).await;
    let bound = shared_calendars(&token, &shared, day).await;
    let mine = calendars_of(&token, MailboxPrincipal::Me, day);
    let theirs = default_calendar(&bound).await;
    let my_calendar = default_calendar(&mine).await;
    let (uid, title) = unique("accept");
    let draft = EventDraft::new(
        my_calendar.id.clone(),
        Uid::new(&uid).unwrap(),
        &title,
        at(day, 16),
        at(day, 17),
        "2026-11-11T10:00:00Z".parse().unwrap(),
    )
    .meeting(MeetingDraft::new(
        SchedulingIdentity::new(CalendarAddress::parse(&me).unwrap()),
        vec![Invitee::required(SchedulingIdentity::new(
            CalendarAddress::parse(&shared).unwrap(),
        ))],
    ));
    mine.create_event(&account(), &draft)
        .await
        .expect("the delegate invites the shared mailbox");

    let outcome = async {
        let invitation = await_event(&bound, &theirs, &title, "the shared mailbox's copy").await;
        bound
            .rsvp_event(
                &account(),
                &invitation,
                &EventRsvp::to(&invitation, &shared, RsvpResponse::Accepted),
            )
            .await
            .map_err(|e| format!("accept on behalf: {e}"))?;
        for _ in 0..30 {
            if let Some(copy) = find(&mine, &my_calendar, &title).await
                && status_of(&copy, &shared) == Some(ParticipationStatus::Accepted)
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err("the organiser's copy never heard the acceptance".to_owned())
    }
    .await;
    delete_titled(&mine, &my_calendar, &title).await;
    delete_titled(&bound, &theirs, &title).await;
    outcome.unwrap();
}

/// A standalone Teams meeting made through a client rooted at the shared mailbox is the
/// **signed-in user's**: Graph makes online meetings for `/me` alone. Proven by the delegate's
/// own, unbound `TeamsMeetings` finding and deleting it.
#[tokio::test]
async fn live_a_standalone_teams_meeting_from_a_bound_client_is_the_delegates() {
    let Some((token, shared)) =
        gated("live_a_standalone_teams_meeting_from_a_bound_client_is_the_delegates")
    else {
        return;
    };
    let handle = engine_core::ids::SharedAccountId::try_from(shared.as_str()).unwrap();
    let bound = TeamsMeetings::new(client(&token, MailboxPrincipal::shared(&handle).unwrap()));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let request = MeetingRequest {
        key: engine_provider::MeetingKey::new(format!("pimshared{nanos:0>18}")).unwrap(),
        title: "PIM live shared-calendar Teams meeting".into(),
        start: "2027-02-14T09:00:00Z".parse().unwrap(),
        end: "2027-02-14T09:30:00Z".parse().unwrap(),
    };
    let made = bound.create_meeting(&request).await.expect("create");
    let own = TeamsMeetings::new(client(&token, MailboxPrincipal::Me));
    own.delete_meeting(&made).await.expect("the delegate's own");
    // Gone: a repeated request makes a new one, which is then cleaned up too.
    let again = own.create_meeting(&request).await.expect("create again");
    assert_ne!(again.join(), made.join(), "the delete reached the meeting");
    own.delete_meeting(&again).await.expect("clean up");
}
