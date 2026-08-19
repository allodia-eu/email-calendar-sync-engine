//! Gated live integration: sending an **iMIP scheduling message** over SMTP, against the
//! Stalwart harness.
//!
//! This is the delivery half of issue #105. When a calendar server does not schedule for
//! the account (`Capabilities::calendar_scheduling` is false — the SabreDAV fixture, and
//! any plain RFC 4791 server), rewriting `PARTSTAT` tells the organizer nothing; the caller
//! has to send the iTIP `REPLY` itself, as a conformant iMIP message (RFC 6047).
//!
//! **Why a real server is not optional.** The offline suite proves what `engine-rfc5322`
//! assembles. It cannot prove that an MTA accepts those bytes, nor that an IMAP server
//! stores and returns them unchanged — and "the sender saw no error" is precisely the
//! failure mode of getting an iMIP message wrong. So the message goes out through the real
//! SMTP submission path, is read back from the server, and is handed to the engine's own
//! inbound bridge (`engine_mime::extract_calendar_part`) — the same code that decides
//! whether an arriving message is an invitation.
//!
//! ## What this does not prove, and why
//!
//! It reads the copy back from **Alice's own Sent folder**, not from the recipient's
//! mailbox, so **inter-account delivery is not covered here**. That is a limitation of the
//! harness, not a choice: this Stalwart fixture accepts a locally-addressed message
//! (`250 … queued`) and never delivers it to the other account's INBOX. Verified with a
//! plain, calendar-free control message, so it is not a property of the iMIP shape — the
//! scheduling suites do not hit it because Stalwart's *internal* iTIP mailer is a different
//! path from the SMTP queue.
//!
//! What is covered is still the part that could plausibly break: a real MTA accepted the
//! `DATA`, and a real IMAP server round-tripped a `multipart/alternative` whose
//! `text/calendar` part keeps its `method=` parameter and its transfer encoding.
//!
//! Skips with no `STALWART_IMAP_ADDR`.

use core::time::Duration;
use std::{collections::BTreeSet, time::Duration as StdDuration};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, Message, StoredContent, StoredState},
    membership::Memberships,
    scheduling::ScheduleMethod,
    sync::SyncUpdate,
    version::RevisionTokens,
};
use engine_provider::{Draft, DraftCalendar, MailEdit, Provider};
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{IgnoreCommits, StreamTuning, submit_mail, sync_mail};
use provider_imap::{ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio_rustls::{TlsConnector, client::TlsStream};

/// The `Message-ID` of the reply this test sends, and finds again in Sent.
const REPLY_MESSAGE_ID: &str = "imip-live-reply@test.local";

/// The iTIP `REPLY` a caller builds when its calendar server will not schedule: the answer,
/// keyed to the organizer's event by `UID` + `SEQUENCE` (RFC 5546 §3.2.3).
const REPLY_ICAL: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Engine//Live//EN\r\n\
                          METHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:imip-live-reply@test.local\r\n\
                          DTSTAMP:20260501T080000Z\r\n\
                          DTSTART;TZID=Europe/Amsterdam:20260604T090000\r\n\
                          ORGANIZER;CN=Bob:mailto:bob@test.local\r\n\
                          ATTENDEE;CN=Alice;PARTSTAT=ACCEPTED:mailto:alice@test.local\r\n\
                          SEQUENCE:0\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// Accepts the harness's self-signed certificate. Test-only; never a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// Connects an `ImapProvider` bound to `mailbox`, optionally with SMTP submission enabled.
async fn connect(
    harness: &Harness,
    mailbox: &str,
    submit: bool,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let mut config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    );
    if submit {
        config = config.with_smtp(harness.smtp_addr.as_str());
    }
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

/// The account's real Sent folder (its `\Sent` SPECIAL-USE name — "Sent Items" on
/// Stalwart), so submission files its copy where this test then re-syncs it from.
async fn sent_folder(harness: &Harness) -> String {
    let provider = connect(harness, "INBOX", false).await;
    let sync = provider
        .sync_mailboxes(&AccountId::try_from("imip-resolve").unwrap(), None)
        .await
        .expect("list folders");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        return "Sent".to_owned();
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role.as_ref() == Some(&MailboxRole::Sent))
        .map_or_else(|| "Sent".to_owned(), |mailbox| mailbox.name)
}

/// Every filed copy carrying this test's `Message-ID`.
///
/// Plural on purpose: an interrupted run leaves residue, and a scenario that took the
/// *first* match would then assert against a copy some earlier run sent. Both the
/// pre-clean and the assertion go through this, so neither can be fooled by a leftover.
async fn filed_copies(
    provider: &ImapProvider<TlsStream<tokio::net::TcpStream>>,
    store: &SqliteStore<ManualClock>,
    account: &AccountId,
) -> Vec<StoredContent> {
    sync_mail(
        core::slice::from_ref(provider),
        store,
        account,
        WorkerId::new("imip-live"),
        Duration::from_mins(5),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    let scope = provider.email_scope(account);
    let mut found = Vec::new();
    for key in store.object_keys(&scope).await.expect("object keys") {
        let payload = store
            .object_payload(&scope, &key)
            .await
            .expect("payload")
            .expect("present");
        // The payload, which is the half that carries the envelope this looks for.
        let content: StoredContent = serde_json::from_value(payload).expect("stored content");
        if content
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == REPLY_MESSAGE_ID)
        {
            found.push(content);
        }
    }
    found
}

/// Removes every prior copy of this test's message, so what it asserts on afterwards can
/// only be the copy it just sent.
async fn pre_clean(
    provider: &ImapProvider<TlsStream<tokio::net::TcpStream>>,
    store: &SqliteStore<ManualClock>,
    account: &AccountId,
) {
    for stale in filed_copies(provider, store, account).await {
        provider
            .edit_mail(account, &MailEdit::delete(stale.id.key().clone()))
            .await
            .expect("remove residue from an earlier run");
    }
}

#[tokio::test]
async fn live_smtp_sends_a_processable_imip_reply() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_smtp_sends_a_processable_imip_reply: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let sent = sent_folder(&harness).await;
    let provider = connect(&harness, &sent, true).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imip-live").unwrap();

    // The capability a host reads before composing: this transport assembles the whole
    // RFC 5322 message, so it owns the `method=` parameter. (JMAP does not, and refuses.)
    let caps = provider.connection_info().capabilities;
    assert!(caps.submission() && caps.scheduling_submission());

    // Residue from an interrupted run would otherwise be indistinguishable from what this
    // run sends — and a stale *good* copy would mask a broken assembler entirely.
    pre_clean(&provider, &store, &account).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("a store holding no pre-clean state");

    // The answer as a host states it: an ordinary draft, plus the iTIP object.
    let draft = Draft::new(
        MessageIdHeader::new(REPLY_MESSAGE_ID).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("bob@test.local")],
        "Accepted: Sprint planning",
        "Alice has accepted this invitation.",
    )
    .with_calendar(DraftCalendar::new(ScheduleMethod::Reply, REPLY_ICAL));

    // The claim under test starts here: a real MTA accepts an iMIP message. A `DATA` the
    // server refuses — for the `method=` parameter, the base64 part, anything — fails this.
    submit_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imip-live"),
        Duration::from_mins(5),
        &draft,
    )
    .await
    .expect("the MTA accepted an iMIP message");

    // …and a real IMAP server stored and returned it unchanged.
    let mut copies = filed_copies(&provider, &store, &account).await;
    assert_eq!(copies.len(), 1, "exactly the copy this run filed");
    // Rebuilt into a whole message because that is what the provider port takes. An IMAP
    // object's filing is its identity — the key embeds the mailbox — so the one this was
    // filed into is the whole of its membership.
    let filed = Message::from_parts(
        copies.remove(0),
        StoredState {
            mailboxes: Memberships::of_one(MailboxId::try_from(sent.as_str()).unwrap()),
            keywords: BTreeSet::new(),
            thread: None,
            revisions: RevisionTokens::none(),
            last_modified: None,
        },
    );
    let raw = provider
        .fetch_message_source(&account, &filed)
        .await
        .expect("fetch the filed source");
    let text = String::from_utf8_lossy(raw.as_bytes()).into_owned();
    assert!(
        text.contains("method=REPLY"),
        "the stored message lost its method= parameter, so it is no longer a scheduling \
         message (RFC 6047 §2.4 note 2):\n{text}"
    );

    // The engine's own inbound bridge — the code that decides whether an arriving message
    // is an invitation — recognizes it, as a **body part** rather than a file.
    let part = engine_mime::extract_calendar_part(&raw)
        .expect("the message carries a detectable scheduling part");
    assert_eq!(part.media_type(), "text/calendar");
    assert!(
        part.from_inline_body(),
        "an iTIP object must be an alternative body part, not an attachment — a disposition \
         here is what gets an answer filed as invite.ics instead of processed"
    );
    assert_eq!(
        part.text(),
        REPLY_ICAL,
        "the iCalendar object must survive the transfer encoding byte for byte; a re-folded \
         or re-wrapped content line is a different object"
    );

    // Clean up, so Sent does not accumulate a copy per run.
    provider
        .edit_mail(&account, &MailEdit::delete(filed.id.key().clone()))
        .await
        .expect("remove the filed copy");
}
