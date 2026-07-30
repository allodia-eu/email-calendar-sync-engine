//! `Engine::message_scheduling` against a **real server-authored iMIP invitation**.
//!
//! The inline tests beside the read use invitations we wrote ourselves, which proves the
//! logic but not the shape: every hand-written fixture is a guess about what arrives. This
//! one is the message Stalwart **generated and mailed** when an organizer stored an event
//! naming an attendee — captured from the harness, trimmed only in two body payloads
//! (`tests/fixtures/README.md`), and it is a good deal nastier than the guesses:
//!
//! - the `text/calendar` part sits at the top level of a `multipart/mixed`, a sibling of a
//!   `multipart/related` wrapping a `multipart/alternative` and an inline PNG — three levels away
//!   from where an iMIP body part is "supposed" to be;
//! - it is **quoted-printable**, so the payload arrives as `=0D=0A`/`=3D` escapes;
//! - its Windows `TZID` is **DQUOTE-quoted** *and* QP-escaped (`TZID=3D"W. Europe Standard Time"`);
//! - its `ATTENDEE` line is folded **mid-`mailto:`** — `mailt`, CRLF, ` o:carol@…`;
//! - and it carries `Content-Disposition: attachment; filename="event.ics"`.
//!
//! The live counterpart is `provider-caldav/tests/scheduling/mod.rs`, which drives the same
//! exchange against the running server; this keeps the observed bytes in the offline suite.

use async_trait::async_trait;
use engine_api::{Engine, MessageAttachment};
use engine_core::{
    ids::{AccountId, MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
    raw::RawMime,
    scheduling::{ScheduleMethod, addresses_match},
};
use engine_provider::{Capabilities, ConnectionInfo, Provider, ProviderResult};

/// The captured invitation, byte-for-byte as committed.
const INVITATION: &[u8] = include_bytes!("fixtures/stalwart-invitation.eml");

/// A provider that serves one fixed raw source, like the body reads' fakes.
struct SourceProvider(&'static [u8]);

#[async_trait]
impl Provider for SourceProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_message_source())
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        _message: &Message,
    ) -> ProviderResult<RawMime> {
        Ok(RawMime::new(self.0.to_vec()))
    }
}

/// The engine, a provider serving the captured invitation, and a message to read it as.
fn fixture() -> (Engine, SourceProvider, AccountId, Message) {
    let engine = Engine::open_in_memory().expect("engine");
    let message = Message::new(
        MessageId::try_from("imap:v1:u1@INBOX").expect("id"),
        Memberships::of_one(MailboxId::try_from("INBOX").expect("mailbox")),
    );
    (
        engine,
        SourceProvider(INVITATION),
        AccountId::try_from("imip-fixture").expect("account"),
        message,
    )
}

#[tokio::test]
async fn a_real_servers_invitation_parses_out_of_its_nested_quoted_printable_part() {
    let (engine, provider, account, message) = fixture();
    let found = engine
        .message_scheduling(&provider, &account, &message)
        .await
        .expect("scheduling read")
        .expect("the invitation is recognized");

    assert_eq!(found.message.method, ScheduleMethod::Request);
    assert_eq!(
        found.message.event.uid.as_str(),
        "caldav-schedule-winzone@test.local"
    );
    assert_eq!(found.message.event.title, "Windows zone invitation");
    assert_eq!(found.message.organizer(), Some("bob@test.local"));
    assert_eq!(found.media_type, "text/calendar");
}

#[tokio::test]
async fn the_servers_quoted_windows_time_zone_resolves_to_iana() {
    // The bug this branch fixed, in the form a real server actually emits it: quoted
    // *and* quoted-printable-escaped. Both layers have to come off before the CLDR lookup,
    // or the event ends up zoned to a name no tzdb resolves and has no instant at all.
    let (engine, provider, account, message) = fixture();
    let found = engine
        .message_scheduling(&provider, &account, &message)
        .await
        .expect("scheduling read")
        .expect("an invitation");

    let zone = found.message.event.start.zone().expect("a zoned start");
    assert!(zone.is_iana(), "got {zone:?}");
    assert_eq!(zone.as_str(), "Europe/Berlin");
    assert!(
        engine_api::resolve_instant(&found.message.event.start).is_ok(),
        "a resolvable zone is what lets the meeting be placed on a grid"
    );
}

#[tokio::test]
async fn the_attendee_folded_across_a_line_break_still_matches_the_delivery_address() {
    // `ATTENDEE;…:mailt` + CRLF + ` o:carol@test.local`. Unfold wrongly and the address is
    // `mailt`, which matches nobody — so the invitation silently stops being *mine* and no
    // RSVP is ever offered.
    let (engine, provider, account, message) = fixture();
    let found = engine
        .message_scheduling(&provider, &account, &message)
        .await
        .expect("scheduling read")
        .expect("an invitation");

    let attendees: Vec<&str> = found
        .message
        .event
        .participants
        .iter()
        .filter_map(|p| p.email.as_deref())
        .collect();
    assert!(
        attendees.contains(&"carol@test.local"),
        "the folded ATTENDEE rejoined into a whole address: {attendees:?}"
    );

    // And the two-condition gate closes: the MTA's own `Delivered-To` names this mailbox,
    // and an ATTENDEE matches it.
    assert_eq!(
        found.delivery_recipients.first().map(String::as_str),
        Some("carol@test.local"),
        "Delivered-To is the header the MTA wrote, and it comes first"
    );
    assert!(attendees.iter().any(|attendee| {
        found
            .delivery_recipients
            .iter()
            .any(|mine| addresses_match(attendee, mine))
    }));
}

#[tokio::test]
async fn a_genuine_request_can_arrive_as_a_dispositioned_attachment() {
    // The observed truth that contradicts the tidy story: this is a real `METHOD:REQUEST`
    // from a real auto-schedule server, and it is `Content-Disposition: attachment;
    // filename="event.ics"` — not an undispositioned `multipart/alternative` body part.
    //
    // So `from_inline_body` answers "was this a body part", and **only** that. A host must
    // not read `false` as "not an invitation": the RSVP gate is a scheduling `METHOD` plus
    // an `ATTENDEE` that is one of the account's own addresses, both of which hold here.
    // Keying the gate on this flag instead would drop every Stalwart invitation on the
    // floor.
    let (engine, provider, account, message) = fixture();
    let found = engine
        .message_scheduling(&provider, &account, &message)
        .await
        .expect("scheduling read")
        .expect("an invitation");

    assert!(
        !found.from_inline_body,
        "the fallback to a dispositioned text/calendar part is what makes this readable"
    );

    // The visible consequence, locked so it is a decision rather than a surprise: because
    // the server dispositioned it, `event.ics` stays in the attachment list. The
    // suppression rule on this branch hides the *undispositioned* iMIP body part (the
    // Gmail/Outlook shape); it does not — and must not — hide a file the sender marked as
    // one.
    let attachments = engine
        .message_attachments(&provider, &account, &message)
        .await
        .expect("attachment listing");
    let names: Vec<&str> = attachments
        .iter()
        .map(MessageAttachment::file_name)
        .collect();
    assert!(
        names.contains(&"event.ics"),
        "a dispositioned calendar file keeps its chip: {names:?}"
    );
}
