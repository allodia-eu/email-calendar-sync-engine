//! End-to-end calendar search: title free text, attendee/organizer/rsvp/calendar
//! structured filters, conference and location scopes, and the occurrence
//! time-range filter over materialized occurrences.

use engine_core::{
    calendar::{
        Event, Location, Participant, ParticipantRole, ParticipationStatus, VirtualLocation,
    },
    ids::{CalendarId, EventId, Uid},
    search_index::{OwnerAddresses, project_event},
};
use engine_search::CalendarQuery;
use engine_store::{OccurrenceRow, TzdataVersion};

use super::*;

#[tokio::test]
async fn calendar_attendee_and_text_search() {
    let store = store();
    let scope = calendar_scope();

    let mut standup = Event::new(
        EventId::try_from("e-standup").unwrap(),
        Uid::new("uid-standup").unwrap(),
        Memberships::of_one(CalendarId::try_from("work").unwrap()),
        CalendarDateTime::Zoned {
            local: LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
    );
    standup.title = "Team standup".to_owned();
    let mut carol = Participant::attendee("carol@example.com");
    carol.roles.insert(ParticipantRole::Attendee);
    carol.participation_status = ParticipationStatus::Accepted;
    standup.participants = vec![carol];

    let lunch = Event::new(
        EventId::try_from("e-lunch").unwrap(),
        Uid::new("uid-lunch").unwrap(),
        Memberships::of_one(CalendarId::try_from("work").unwrap()),
        CalendarDateTime::Zoned {
            local: LocalDateTime::new(2026, 6, 1, 12, 0, 0).unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
    );

    let owner = OwnerAddresses::new(["me@example.com"]);
    let claim = store
        .claim_sync_scope(account(), &scope, lease())
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.push_event(project_event(&standup, &owner));
    derived.push_event(project_event(&lunch, &owner));
    let update = SyncUpdate::delta(vec![standup, lunch], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("c1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    // Free text on the title.
    let by_text = store
        .search_calendar(
            std::slice::from_ref(&scope),
            &CalendarQuery::parse("standup").unwrap(),
            10,
        )
        .await
        .unwrap();
    assert_eq!(by_text.keys().len(), 1);
    assert_eq!(by_text.keys()[0].as_str(), "e-standup");

    // Attendee junction filter.
    let by_attendee = store
        .search_calendar(
            &[scope],
            &CalendarQuery::parse("attendee:carol@example.com").unwrap(),
            10,
        )
        .await
        .unwrap();
    assert_eq!(by_attendee.keys().len(), 1);
    assert_eq!(by_attendee.keys()[0].as_str(), "e-standup");
}

#[tokio::test]
async fn calendar_structured_filters_and_occurrence_range() {
    let store = store();
    let scope = calendar_scope();
    let owner = OwnerAddresses::new(["me@example.com"]);

    let mut review = Event::new(
        EventId::try_from("e1").unwrap(),
        Uid::new("u1").unwrap(),
        Memberships::of_one(CalendarId::try_from("work").unwrap()),
        zoned(2026, 6, 1, 9),
    );
    review.title = "Review".to_owned();
    let mut me = Participant::attendee("me@example.com");
    me.roles.insert(ParticipantRole::Owner);
    me.participation_status = ParticipationStatus::Accepted;
    review.participants = vec![me];
    review.virtual_locations = vec![VirtualLocation::new("https://meet.example/x")];
    review.locations = vec![Location::named("Boardroom")];

    let other = Event::new(
        EventId::try_from("e2").unwrap(),
        Uid::new("u2").unwrap(),
        Memberships::of_one(CalendarId::try_from("personal").unwrap()),
        zoned(2026, 6, 2, 9),
    );

    let claim = store
        .claim_sync_scope(account(), &scope, lease())
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.push_event(project_event(&review, &owner));
    derived.push_event(project_event(&other, &owner));
    // project_event does not expand recurrence; materialize occurrences directly.
    derived.occurrences.push(OccurrenceRow {
        event: pk("e1"),
        start: "2026-06-01T07:00:00Z".parse().unwrap(),
        end: "2026-06-01T07:30:00Z".parse().unwrap(),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025b"),
    });
    derived.occurrences.push(OccurrenceRow {
        event: pk("e2"),
        start: "2026-06-02T07:00:00Z".parse().unwrap(),
        end: "2026-06-02T07:30:00Z".parse().unwrap(),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025b"),
    });
    let update = SyncUpdate::delta(vec![review, other], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("c1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    // organizer, rsvp, calendar membership, conference, and the location: text
    // scope each single out e1.
    for query in [
        "organizer:me@example.com",
        "rsvp:accepted",
        "calendar:work",
        "has_conference:true",
        "location:boardroom",
    ] {
        let results = store
            .search_calendar(
                std::slice::from_ref(&scope),
                &CalendarQuery::parse(query).unwrap(),
                10,
            )
            .await
            .unwrap();
        assert_eq!(results.keys().len(), 1, "query {query}");
        assert_eq!(results.keys()[0].as_str(), "e1", "query {query}");
    }

    // The occurrence time-range covers only Jun 1, so e2 (Jun 2) is excluded.
    let ranged = store
        .search_calendar(
            std::slice::from_ref(&scope),
            &CalendarQuery::parse("after:2026-06-01 before:2026-06-02").unwrap(),
            10,
        )
        .await
        .unwrap();
    assert_eq!(ranged.keys().len(), 1);
    assert_eq!(ranged.keys()[0].as_str(), "e1");

    // No scopes → empty (exercises the calendar empty-scope guard).
    let none = store
        .search_calendar(&[], &CalendarQuery::parse("review").unwrap(), 10)
        .await
        .unwrap();
    assert!(none.hits.is_empty());
}
