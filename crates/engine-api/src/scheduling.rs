//! The host-facing inbound-scheduling read (`Engine::message_scheduling`) — the one call
//! that turns a message into "this is a meeting invitation".
//!
//! Kept in its own module beside [`body`](crate::body) rather than growing `engine.rs`; it is
//! another `impl Engine` block over the same store.

use engine_core::{ids::AccountId, mail::Message, scheduling::SchedulingMessage};
use engine_ical::parse_scheduling_message;
use engine_provider::Provider;
use engine_sync::fetch_message_scheduling;

use crate::{ApiError, Engine, engine::map_sync_error};

/// An inbound iTIP scheduling message found in a mail message, with the mail-path context a
/// host needs to decide what to show.
///
/// This is deliberately *not* a decision — it reports what arrived. Whether to offer an RSVP
/// is a product rule over `message.method` plus an attendee match, and it lives above this
/// facade (the two-condition gate: a scheduling `METHOD`, **and** an `ATTENDEE` that is one of
/// the account's own addresses).
#[derive(Debug, Clone)]
pub struct InboundScheduling {
    /// The parsed iTIP message: its [`ScheduleMethod`](engine_core::scheduling::ScheduleMethod),
    /// the carried event, and the `DTSTAMP`/`SEQUENCE` that key supersession.
    pub message: SchedulingMessage,
    /// The part's declared media type, for diagnostics.
    pub media_type: String,
    /// Whether the payload came from an alternative **body** part (the iMIP shape) rather
    /// than a file the sender attached.
    ///
    /// A body part means "this message *is* an invitation"; an attached `.ics` with
    /// `METHOD:PUBLISH` means "here is a calendar file", which is why a published `.ics`
    /// keeps its attachment chip and offers no RSVP.
    pub from_inline_body: bool,
    /// The addresses this message was delivered to — the MTA delivery headers
    /// (`Delivered-To`/`X-Original-To`/`Envelope-To`) first, then `To:`/`Cc:`.
    ///
    /// This is how an invitation sent to an **alias** is recognized with no configuration at
    /// all: the message was delivered to this mailbox, so an `ATTENDEE` matching one of these
    /// is the user, even when the account's primary address is different. Compare with
    /// [`addresses_match`](engine_core::scheduling::addresses_match) — never with `==`.
    pub delivery_recipients: Vec<String>,
}

impl Engine {
    /// Returns the iTIP scheduling message `message` carries, or `None` when it carries none.
    ///
    /// Finds the `text/calendar` payload in the MIME tree, decodes it (base64,
    /// quoted-printable, and the declared charset all appear in the wild), and parses it with
    /// the engine's single hardened iCalendar parser. Cache-first on the raw RFC 5322 source,
    /// so a message whose body was already read costs **no** extra provider fetch — and the
    /// delivery recipients come out of the same pass.
    ///
    /// `Ok(None)` covers both "not an invitation" and "carried something that would not
    /// parse": a malformed calendar part is not an error a user can act on, and it must never
    /// keep the message itself from being read. The parse failure is swallowed here by
    /// design; the reading view still shows the mail.
    ///
    /// This read takes **no** lease, so it never contends with an in-flight sync.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the provider fetch fails (a stale IMAP target is a
    /// `Conflict` — re-sync, then retry) or the store cache read fails.
    pub async fn message_scheduling<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        message: &Message,
    ) -> Result<Option<InboundScheduling>, ApiError> {
        let found = fetch_message_scheduling(provider, &self.store, account, message)
            .await
            .map_err(map_sync_error)?;
        let Some((part, delivery_recipients)) = found else {
            return Ok(None);
        };
        // A body that does not parse is not an invitation, and not a failure of this read:
        // the message is still perfectly readable mail.
        let Ok(scheduling) = parse_scheduling_message(part.text()) else {
            return Ok(None);
        };
        Ok(Some(InboundScheduling {
            message: scheduling,
            media_type: part.media_type().to_owned(),
            from_inline_body: part.from_inline_body(),
            delivery_recipients,
        }))
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use engine_core::{
        ids::{AccountId, MailboxId, MessageId},
        membership::Memberships,
        raw::RawMime,
        scheduling::{ScheduleMethod, addresses_match},
    };
    use engine_provider::{Capabilities, ConnectionInfo, Provider, ProviderResult};

    use crate::Engine;

    /// A Microsoft invitation: the calendar payload is an alternative body part with a
    /// **Windows** `TZID`, and the alias it was delivered to appears only in `Delivered-To`.
    const OUTLOOK_INVITE: &[u8] = b"From: boss@test.local\r\n\
        To: everyone@test.local\r\n\
        Delivered-To: info@test.local\r\n\
        Subject: Sprint planning\r\n\
        Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
        --a\r\nContent-Type: text/plain\r\n\r\nWhen: 30 July\r\n\
        --a\r\nContent-Type: text/calendar; charset=\"utf-8\"; method=REQUEST\r\n\r\n\
        BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
        UID:meeting-1@test.local\r\nDTSTAMP:20260701T080000Z\r\n\
        DTSTART;TZID=W. Europe Standard Time:20260730T163000\r\n\
        DTEND;TZID=W. Europe Standard Time:20260730T173000\r\n\
        SUMMARY:Sprint planning\r\nSEQUENCE:0\r\n\
        ORGANIZER;CN=Boss:mailto:boss@test.local\r\n\
        ATTENDEE;CN=Info;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:info@test.local\r\n\
        END:VEVENT\r\nEND:VCALENDAR\r\n\
        --a--\r\n";

    const PLAIN_MAIL: &[u8] = b"From: a@test.local\r\nTo: b@test.local\r\n\
        Content-Type: text/plain\r\n\r\njust mail\r\n";

    /// A provider serving one fixed raw source, like the body reads' fakes.
    struct SourceProvider {
        caps: Capabilities,
        raw: Vec<u8>,
    }

    #[async_trait]
    impl Provider for SourceProvider {
        fn connection_info(&self) -> ConnectionInfo {
            ConnectionInfo::new(self.caps)
        }

        async fn fetch_message_source(
            &self,
            _account: &AccountId,
            _message: &engine_core::mail::Message,
        ) -> ProviderResult<RawMime> {
            Ok(RawMime::new(self.raw.clone()))
        }
    }

    async fn scheduling_of(raw: &[u8]) -> Option<super::InboundScheduling> {
        let engine = Engine::open_in_memory().expect("engine");
        let provider = SourceProvider {
            caps: Capabilities::none().with_mail().with_message_source(),
            raw: raw.to_vec(),
        };
        let account = AccountId::try_from("acct").expect("account");
        let message = engine_core::mail::Message::new(
            MessageId::try_from("imap:v1:u1@INBOX").expect("id"),
            Memberships::of_one(MailboxId::try_from("INBOX").expect("mailbox")),
        );
        engine
            .message_scheduling(&provider, &account, &message)
            .await
            .expect("scheduling read")
    }

    #[tokio::test]
    async fn an_outlook_invitation_parses_including_its_windows_timezone() {
        let found = scheduling_of(OUTLOOK_INVITE).await.expect("an invitation");
        assert_eq!(found.message.method, ScheduleMethod::Request);
        assert_eq!(found.message.event.title, "Sprint planning");
        assert_eq!(found.message.organizer(), Some("boss@test.local"));
        assert!(
            found.from_inline_body,
            "the iMIP payload is a body part, so no attachment chip belongs on it"
        );

        // G3: the Windows TZID must have resolved to a real IANA zone, or the meeting has no
        // instant and can be neither placed nor checked for conflicts.
        let zone = found.message.event.start.zone().expect("a zoned start");
        assert!(zone.is_iana(), "got {zone:?}");
        assert_eq!(zone.as_str(), "Europe/Berlin");
    }

    #[tokio::test]
    async fn the_alias_it_was_delivered_to_is_offered_for_attendee_matching() {
        // D5, with no configuration: the account's primary address is not in this message at
        // all — `To:` names a list — but `Delivered-To` names the alias that is also the
        // ATTENDEE, so the invitation is recognizably *for me*.
        let found = scheduling_of(OUTLOOK_INVITE).await.expect("an invitation");
        assert_eq!(
            found.delivery_recipients.first().unwrap(),
            "info@test.local"
        );

        let attendee_addresses: Vec<&str> = found
            .message
            .event
            .participants
            .iter()
            .filter_map(|p| p.email.as_deref())
            .collect();
        assert!(
            attendee_addresses.iter().any(|attendee| found
                .delivery_recipients
                .iter()
                .any(|mine| addresses_match(attendee, mine))),
            "an ATTENDEE must match a delivery recipient: {attendee_addresses:?}"
        );
    }

    #[tokio::test]
    async fn ordinary_mail_carries_no_scheduling_message() {
        assert!(scheduling_of(PLAIN_MAIL).await.is_none());
    }

    #[tokio::test]
    async fn an_unparseable_calendar_part_does_not_fail_the_read() {
        // A malformed payload is not an error the user can act on, and it must never stop the
        // message from being read.
        let broken = b"To: a@test.local\r\n\
            Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
            --a\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
            --a\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nnonsense\r\n\
            --a--\r\n";
        assert!(scheduling_of(broken).await.is_none());
    }
}
