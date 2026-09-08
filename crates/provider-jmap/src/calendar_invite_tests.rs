//! JMAP meeting creation request tests.

use engine_core::error::FailureClass;
use engine_provider::{
    CalendarAddress, CalendarWrites, EventDraft, EventPatch, Invitee, InviteePatch, MeetingDraft,
    PatchTarget, SchedulingIdentity,
};
use serde_json::{Value, json};

use super::{calendar_write_support::*, calendar_write_tests::invited, provider_test_support::*};

#[tokio::test]
async fn create_posts_current_jmap_participants() {
    let (provider, executor) = recording(vec![
        fixture("participant_identity_get_response.json"),
        set_response(&json!({ "created": { "new": { "id": EVENT } } })),
    ]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        stamp(),
    )
    .meeting(MeetingDraft::new(
        SchedulingIdentity::named(CalendarAddress::parse("BOB@test.local").unwrap(), "Owner"),
        vec![Invitee::required(SchedulingIdentity::new(
            CalendarAddress::parse("guest@example.com").unwrap(),
        ))],
    ));

    provider.create_event(&account(), &draft).await.unwrap();

    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["methodCalls"][0][0], "ParticipantIdentity/get");
    assert_eq!(requests[0]["methodCalls"][0][1]["ids"], Value::Null);
    let args = &requests[1]["methodCalls"][0][1];
    let participants = &args["create"]["new"]["participants"];
    assert_eq!(
        args["create"]["new"]["organizerCalendarAddress"],
        "mailto:BOB@test.local"
    );
    assert_eq!(
        participants["a"]["calendarAddress"],
        "mailto:BOB@test.local"
    );
    assert_eq!(participants["a"]["roles"]["owner"], true);
    assert_eq!(participants["a"]["roles"]["chair"], true);
    assert_eq!(participants["a"]["participationStatus"], "accepted");
    assert_eq!(
        participants["invitee-1"]["calendarAddress"],
        "mailto:guest@example.com"
    );
    assert_eq!(participants["invitee-1"]["roles"]["required"], true);
    assert_eq!(participants["invitee-1"]["expectReply"], true);
}

#[tokio::test]
async fn create_refuses_an_organiser_the_account_does_not_own() {
    let (provider, executor) = recording(vec![json!({
        "methodResponses": [["ParticipantIdentity/get", {
            "accountId": "c",
            "state": "identity-state",
            "list": [{
                "id": "identity-1",
                "calendarAddress": "mailto:other@example.com",
                "isDefault": true,
            }],
        }, "0"]],
    })]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        stamp(),
    )
    .meeting(MeetingDraft::new(
        SchedulingIdentity::new(CalendarAddress::parse("owner@example.com").unwrap()),
        vec![Invitee::required(SchedulingIdentity::new(
            CalendarAddress::parse("guest@example.com").unwrap(),
        ))],
    ));

    let error = provider.create_event(&account(), &draft).await.unwrap_err();

    assert_eq!(error.class(), FailureClass::Permanent);
    assert_eq!(
        executor.request_count(),
        1,
        "no create may follow the refusal"
    );
}

#[tokio::test]
async fn create_refuses_more_participants_than_the_account_advertises() {
    let (provider, executor) = recording(Vec::new());
    let invitees = (0..20)
        .map(|index| {
            Invitee::required(SchedulingIdentity::new(
                CalendarAddress::parse(&format!("guest{index}@example.com")).unwrap(),
            ))
        })
        .collect();
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        stamp(),
    )
    .meeting(MeetingDraft::new(
        SchedulingIdentity::new(CalendarAddress::parse("owner@example.com").unwrap()),
        invitees,
    ));

    let error = provider.create_event(&account(), &draft).await.unwrap_err();

    assert_eq!(error.class(), FailureClass::Permanent);
    assert_eq!(executor.request_count(), 0);
}

#[tokio::test]
async fn invitee_patch_preserves_opaque_ids_and_existing_responses() {
    let (provider, executor) = recording(vec![set_response(
        &json!({ "updated": { EVENT: Value::Null } }),
    )]);
    let base = invited();
    let patch = EventPatch::new(stamp()).invitees(
        InviteePatch::new()
            .upsert(Invitee::optional(SchedulingIdentity::new(
                CalendarAddress::parse("info@example.com").unwrap(),
            )))
            .upsert(Invitee::required(SchedulingIdentity::new(
                CalendarAddress::parse("new@example.com").unwrap(),
            ))),
    );

    provider
        .patch_event(&account(), &base, &edit(&base, PatchTarget::Series, patch))
        .await
        .unwrap();

    let (_, _, args) = executor.sole_call();
    let participants = &args["update"][EVENT]["participants"];
    assert_eq!(participants["9f0c"]["participationStatus"], "needs-action");
    assert_eq!(participants["9f0c"]["roles"]["optional"], true);
    assert_eq!(participants["d7a1"]["roles"]["owner"], true);
    assert_eq!(
        participants["invitee-1"]["calendarAddress"],
        "mailto:new@example.com"
    );
}

#[tokio::test]
async fn invitee_patch_respects_the_accounts_participant_limit() {
    let (provider, executor) = recording(Vec::new());
    let base = invited();
    let patch = (0..19).fold(InviteePatch::new(), |patch, index| {
        patch.upsert(Invitee::required(SchedulingIdentity::new(
            CalendarAddress::parse(&format!("guest{index}@example.com")).unwrap(),
        )))
    });

    let error = provider
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).invitees(patch),
            ),
        )
        .await
        .unwrap_err();

    assert_eq!(error.class(), FailureClass::Permanent);
    assert_eq!(executor.request_count(), 0);
}
