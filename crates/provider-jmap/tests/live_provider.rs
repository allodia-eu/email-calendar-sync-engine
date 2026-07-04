//! Gated live provider-level checks against the Stalwart harness: session
//! discovery, mail/calendar fetch, and submission through the real HTTP client.
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green. The
//! full sync-loop-through-store integration is in `live_sync.rs`.

use std::time::Duration;

use engine_core::ids::{AccountId, MailboxId, MessageIdHeader};
use engine_core::mail::{EmailAddress, Message, SystemKeyword};
use engine_core::sync::{JmapDataType, SyncUpdate};
use engine_provider::{Draft, MailEdit, Provider, WatchEvent};
use provider_jmap::{Credentials, JmapClient, JmapConfig, JmapProvider, JmapWatcher};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn config(harness: &Harness) -> JmapConfig {
    JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    )
}

/// Re-syncs mail from scratch and returns the snapshot's messages.
async fn all_messages(provider: &JmapProvider) -> Vec<Message> {
    let emails = provider.sync_email(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("expected snapshot");
    };
    objects
}

/// Finds the message carrying `message_id` in its envelope, if present.
fn find_by_message_id(messages: &[Message], message_id: &str) -> Option<Message> {
    messages
        .iter()
        .find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .cloned()
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(config(harness))
        .await
        .expect("connect")
}

#[tokio::test]
async fn live_session_discovery() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_session_discovery: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let client = JmapClient::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");
    let session = client.session();
    // Capabilities advertised; the API URL was rebased onto the connection origin.
    assert!(session.capabilities().mail());
    assert!(session.capabilities().submission());
    assert!(session.capabilities().calendars());
    assert!(
        session
            .api_url()
            .starts_with(&format!("http://{}", harness.http_addr))
    );
}

#[tokio::test]
async fn live_mail_fetch() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_mail_fetch: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    let mailboxes = provider.sync_mailboxes(&account(), None).await.unwrap();
    assert!(mailboxes.is_snapshot());

    let emails = provider.sync_email(&account(), None).await.unwrap();
    assert!(emails.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &emails.update else {
        panic!("expected snapshot");
    };
    // Assert by seed subject (harness-controlled), not exact count — submission
    // tests file extra items in Sent.
    let subjects: std::collections::BTreeSet<&str> = objects
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    for seed in [
        "Harness baseline message",
        "Duplicate Message-ID (copy A)",
        "Filed under Projects",
    ] {
        assert!(subjects.contains(seed), "seed subject missing: {seed}");
    }

    // A delta from the fresh cursor is empty (nothing changed since).
    let delta = provider
        .sync_email(&account(), Some(&emails.next_cursor))
        .await
        .unwrap();
    assert!(!delta.is_snapshot());
}

#[tokio::test]
async fn live_message_source() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_message_source: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    // Advertised because the session exposes mail + a download template.
    assert!(provider.capabilities().message_source());

    let emails = provider.sync_email(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = &emails.update else {
        panic!("expected snapshot");
    };
    // Pick a known seed message and download its raw RFC 5322 source via blobId.
    let seed = objects
        .iter()
        .find(|m| m.envelope.subject.as_deref() == Some("Harness baseline message"))
        .expect("seed message present");
    assert!(seed.blob_id.is_some(), "synced message carries its blobId");
    let raw = provider
        .fetch_message_source(&account(), seed)
        .await
        .expect("download raw source");
    let text = String::from_utf8_lossy(raw.as_bytes());
    // The downloaded bytes are the real RFC 5322 source — headers + the subject.
    assert!(
        text.contains("Subject: Harness baseline message"),
        "got: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("from:"),
        "raw source has envelope headers: {text}"
    );
}

#[tokio::test]
async fn live_calendar_fetch() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_calendar_fetch: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    assert!(
        provider
            .sync_calendars(&account(), None)
            .await
            .unwrap()
            .is_snapshot()
    );
    let events = provider.sync_events(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected snapshot");
    };
    let uids: std::collections::BTreeSet<&str> = objects.iter().map(|e| e.uid.as_str()).collect();
    for uid in [
        "oneoff-2001@test.local",
        "weekly-2002@test.local",
        "meeting-2003@test.local",
        "virtual-2004@test.local",
        "allday-2005@test.local",
        "floating-2006@test.local",
    ] {
        assert!(uids.contains(uid), "seed event uid missing: {uid}");
    }
}

#[tokio::test]
async fn live_submit_email() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_submit_email: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    let draft = Draft::new(
        MessageIdHeader::new("step4-live-send@test.local").unwrap(),
        EmailAddress::named("Alice", &harness.account),
        vec![EmailAddress::new("bob@test.local")],
        "Step 4 live submission",
        "Sent by the step-4 live submission test.",
    );
    let receipt = provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit");
    assert!(!receipt.email_key.as_str().is_empty());
    assert_eq!(receipt.message_id.as_str(), "step4-live-send@test.local");
}

#[tokio::test]
async fn live_submit_email_with_attachment() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_submit_email_with_attachment: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    let mid = "jmap-attach-probe@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(mid).unwrap(),
        EmailAddress::named("Alice", &harness.account),
        vec![EmailAddress::new("bob@test.local")],
        "JMAP attachment probe",
        "See the attached note.",
    )
    .with_attachment(engine_provider::DraftAttachment::attachment(
        "note.txt",
        "text/plain",
        b"jmap-attachment-live-body".to_vec(),
    ));
    provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit with attachment");

    // The sent copy synced back carries the attachment; its raw source shows the part.
    let sent = find_by_message_id(&all_messages(&provider).await, mid).expect("sent copy synced");
    assert!(
        sent.has_attachment,
        "the synced message reports an attachment"
    );
    let raw = provider
        .fetch_message_source(&account(), &sent)
        .await
        .expect("download raw source");
    let text = String::from_utf8_lossy(raw.as_bytes());
    assert!(
        text.contains("note.txt"),
        "attachment filename in MIME: {text}"
    );
    assert!(
        text.contains("multipart/mixed"),
        "attachment wrapped in multipart/mixed: {text}"
    );

    // Clean up the throwaway sent copy so the seed dataset stays pristine.
    provider
        .edit_mail(&account(), &MailEdit::delete(sent.id.key().clone()))
        .await
        .expect("cleanup delete");
}

#[tokio::test]
async fn live_edit_mail_keyword_move_and_delete() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_edit_mail_keyword_move_and_delete: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;
    assert!(provider.capabilities().mail_writes());

    // Operate on a throwaway message we own (the Sent copy of a fresh submission),
    // so the shared seed dataset the other live tests assert on stays untouched.
    let mid = "jmap-edit-probe@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(mid).unwrap(),
        EmailAddress::named("Alice", &harness.account),
        vec![EmailAddress::new("bob@test.local")],
        "JMAP edit probe",
        "A throwaway message the edit_mail live test mutates.",
    );
    provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit probe");

    // Resolve the Archive mailbox (a move destination) and locate the sent copy.
    let mailboxes = provider.sync_mailboxes(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects: boxes, .. } = mailboxes.update else {
        panic!("expected mailbox snapshot");
    };
    let archive: MailboxId = boxes
        .iter()
        .find(|m| m.name == "Archive")
        .expect("Archive mailbox")
        .id
        .clone();

    let sent = find_by_message_id(&all_messages(&provider).await, mid).expect("sent copy synced");
    let key = sent.id.key().clone();
    assert!(!sent.has_system_keyword(SystemKeyword::Flagged));

    // (1) SetKeywords: flag it, then confirm the flag is visible after re-sync.
    provider
        .edit_mail(&account(), &MailEdit::set_flagged(key.clone(), true))
        .await
        .expect("flag");
    let flagged = find_by_message_id(&all_messages(&provider).await, mid).expect("still present");
    assert!(flagged.has_system_keyword(SystemKeyword::Flagged));

    // (2) MoveTo: move it into Archive; membership becomes exactly {Archive}.
    provider
        .edit_mail(&account(), &MailEdit::move_to(key.clone(), archive.clone()))
        .await
        .expect("move");
    let moved = find_by_message_id(&all_messages(&provider).await, mid).expect("still present");
    assert!(moved.mailboxes.contains(&archive));
    assert_eq!(moved.mailboxes.len().get(), 1);
    // The JMAP id is stable across the move — the same object, new membership.
    assert_eq!(moved.id.key(), &key);

    // (3) Delete: destroy it permanently; the next snapshot no longer carries it.
    provider
        .edit_mail(&account(), &MailEdit::delete(key.clone()))
        .await
        .expect("delete");
    assert!(
        find_by_message_id(&all_messages(&provider).await, mid).is_none(),
        "deleted message must be gone from the snapshot"
    );
}

#[tokio::test]
async fn live_watch_sees_a_change_over_event_source() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_watch_sees_a_change_over_event_source: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");

    // The provider advertises push (an EventSource endpoint).
    let provider = connect(&harness).await;
    assert!(provider.capabilities().idle());

    // Open a dedicated watch stream BEFORE causing the change, so the notification
    // cannot fall into the gap. A short ping keeps the stream lively.
    let mut watcher = JmapWatcher::connect(
        config(&harness),
        &[JmapDataType::Email, JmapDataType::Mailbox],
        Duration::from_secs(5),
    )
    .await
    .expect("open event source");

    // Cause an Email state change on a separate connection: submit a throwaway message.
    let mid = "jmap-watch-probe@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(mid).unwrap(),
        EmailAddress::named("Alice", &harness.account),
        vec![EmailAddress::new("bob@test.local")],
        "JMAP watch probe",
        "Wakes the EventSource watcher.",
    );
    provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit");

    // The open stream delivers a StateChange; the first Changed arrives quickly (an
    // initial baseline state or the submit's change — either is a Changed). Skip ping
    // keep-alives; bound the wait so a broken stream fails fast instead of hanging.
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        // Skip keep-alives; stop on the first real change.
        while watcher.next_event().await.expect("watch event") != WatchEvent::Changed {}
    })
    .await;
    assert!(
        waited.is_ok(),
        "expected a Changed event within the deadline"
    );

    // Clean up the throwaway sent copy so the seed dataset stays pristine.
    if let Some(sent) = find_by_message_id(&all_messages(&provider).await, mid) {
        provider
            .edit_mail(&account(), &MailEdit::delete(sent.id.key().clone()))
            .await
            .expect("cleanup delete");
    }
}
