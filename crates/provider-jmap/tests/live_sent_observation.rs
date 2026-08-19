//! Gated live integration: a message that **enters Sent through a state change** still yields
//! its recipients.
//!
//! This is how a message usually becomes sent, not an edge case. On a provider that files in
//! place, sending does not mint a new object: JMAP's `EmailSubmission/set` moves the existing
//! draft into Sent with `onSuccessUpdateEmail`, and Gmail's `drafts.send` adds `SENT` to the
//! message the draft already had. Either way the next sync sees an update of a key the store is
//! already holding — an `Email/changes` `updated` id, not a `created` — and reading only the
//! whole objects an update carries means the address you just wrote to never enters autosuggest.
//!
//! A state change carries filing and keywords, no envelope, so the recipients come from the
//! message's stored payload. That is engine-side, but the premise it rests on — *a move into
//! Sent reaches us as an update of the same object* — is the server's behaviour, so it is
//! asserted here rather than only against a fake.
//!
//! Operates on the dedicated `JmapSent` mailbox and its own seeded message, whose recipient
//! address (`sent-observer@example.test`) is addressed nowhere else in the seed — so the
//! assertion cannot be satisfied by another fixture, and a concurrently running suite that also
//! touches Sent cannot produce it. The account's real Sent collection is the one destination
//! this test cannot provision (the role is the server's to assign), so it moves in and back out.
//! Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
};
use engine_provider::{MailEdit, Provider};
use engine_store::{ContactStore, MailSelector, ManualClock, StoreRead, WorkerId};
use engine_sync::{IgnoreCommits, StreamTuning, sync_mail};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

type Store = SqliteStore<ManualClock>;

/// The recipient only this fixture addresses.
const OBSERVED: &str = "sent-observer@example.test";

/// The subject of the message this test owns.
const SUBJECT: &str = "JMAP sent-observation subject";

fn worker() -> WorkerId {
    WorkerId::new("jmap-live-sent")
}

/// How many distinct sent messages the store has observed for [`OBSERVED`].
async fn observed_count(store: &Store) -> u64 {
    store
        .recipient_interactions(None)
        .await
        .expect("read recipient interactions")
        .iter()
        .find(|item| item.email.as_str() == OBSERVED)
        .map_or(0, |item| item.sent_count)
}

#[tokio::test]
async fn a_message_moved_into_sent_is_observed_without_ever_being_re_fetched() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_message_moved_into_sent_...: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("jmap-live-sent").unwrap();
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect JMAP");

    let mailboxes = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("mailboxes")
        .update
        .changed()
        .to_vec();
    // The home mailbox is found by its harness-controlled **name**, the destination by its
    // **role** — the engine resolves Sent by role, and the display name is the server's
    // (Stalwart calls it "Sent Items").
    let home: MailboxId = mailboxes
        .iter()
        .find(|m| m.name == "JmapSent")
        .map_or_else(|| panic!("the seeded JmapSent mailbox"), |m| m.id.clone());
    let sent: MailboxId = mailboxes
        .iter()
        .find(|m| m.role == Some(MailboxRole::Sent))
        .map_or_else(|| panic!("the account's Sent collection"), |m| m.id.clone());

    // Put the message back in its home **before the store has seen anything**. An interrupted
    // run leaves it in Sent, and this store is fresh on every run: a first sync that found it
    // there would observe it as a whole object and record exactly the count this test is about
    // to attribute to the state change. So the key is resolved straight from the provider and
    // the reset lands before `sync_mail` runs at all.
    let key = provider
        .sync_email(&account, None)
        .await
        .expect("enumerate mail")
        .update
        .changed()
        .iter()
        .find(|message| message.envelope.subject.as_deref() == Some(SUBJECT))
        .unwrap_or_else(|| panic!("no seeded message with subject {SUBJECT:?}"))
        .id
        .key()
        .clone();
    provider
        .edit_mail(
            &account,
            &MailEdit::MoveTo {
                target: key.clone(),
                destination: home.clone(),
            },
        )
        .await
        .expect("put it back in its home mailbox");

    let first = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    assert!(first.upserted() > 0, "the seed landed");
    assert!(
        store
            .list_mail(
                core::slice::from_ref(&account),
                MailSelector::Keys(core::slice::from_ref(&key)),
                usize::MAX,
            )
            .await
            .expect("list mail")
            .iter()
            .any(|row| row.mailboxes.contains(&home)),
        "the reset landed: the message starts in the mailbox this test owns, not in Sent"
    );

    assert_eq!(
        observed_count(&store).await,
        0,
        "a message that is not in Sent says nothing about who was written to"
    );

    // ---- The send: the message moves into Sent under the same id. ----
    provider
        .edit_mail(
            &account,
            &MailEdit::MoveTo {
                target: key.clone(),
                destination: sent,
            },
        )
        .await
        .expect("move it into Sent");

    let applied = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    assert_eq!(
        applied.upserted(),
        0,
        "the move rewrote no message — which is exactly why the recipients had to come from \
         the stored payload rather than from a whole object the update never carried"
    );
    assert_eq!(
        observed_count(&store).await,
        1,
        "and the address the message was written to is now a suggestion"
    );

    // Replaying the same state leaves the count alone: the observation is keyed by
    // `(account, source message, canonical email)`, so a re-sync cannot inflate it.
    sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    assert_eq!(observed_count(&store).await, 1, "replay does not double it");

    // ---- Put it back, so a re-run starts where this one did. ----
    provider
        .edit_mail(
            &account,
            &MailEdit::MoveTo {
                target: key,
                destination: home,
            },
        )
        .await
        .expect("move it back out of Sent");
}
