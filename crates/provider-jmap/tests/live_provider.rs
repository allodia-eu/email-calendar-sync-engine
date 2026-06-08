//! Gated live provider-level checks against the Stalwart harness: session
//! discovery, mail/calendar fetch, and submission through the real HTTP client.
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green. The
//! full sync-loop-through-store integration is in `live_sync.rs`.

use engine_core::ids::{AccountId, MessageIdHeader};
use engine_core::mail::EmailAddress;
use engine_core::sync::SyncUpdate;
use engine_provider::{Draft, Provider};
use provider_jmap::{Credentials, JmapClient, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
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
