//! Gated live checks for reporting a message against a real Google account.
//!
//! Two Gmail behaviours the adapter depends on are asserted here because only a live
//! call can show them, and both are undocumented:
//!
//! - adding `SPAM` files the message **by itself** — the server drops `INBOX` without being asked,
//!   so there is no separate move for the adapter to make;
//! - removing `SPAM` does **not** put the message back, which is why the not-junk direction adds
//!   the destination explicitly. That one is a trap: the naive implementation makes the message
//!   vanish from the folder the user is looking at.
//!
//! The archiving half has no live test of its own on purpose: a bare
//! `removeLabelIds:["SPAM"]` cannot be expressed through the neutral API — which is the
//! point of the mapping — so it is pinned by the captured fixture and the offline unit
//! test instead. What *is* asserted live is that the adapter's not-junk direction really
//! does land the message back in the Inbox.
//!
//! The test sends its own throwaway message rather than touching whatever is in the
//! account, and cleans up after itself.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_report -- --nocapture --test-threads=1
//! ```

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::EmailAddress,
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, ReportEvidence, ReportVerdict};
use provider_google::{GmailProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The test account's own address — the probe is self-addressed, so nothing leaves the
/// mailbox.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
}

/// The label membership of `key` as the engine sees it.
///
/// Panics when the snapshot does not carry the message: a junk report files it into
/// `SPAM`, and the snapshot asks for `SPAM` (`fetch::list_url`), so a message missing
/// here is a regression rather than an expected disappearance —
/// `live_spam_trash.rs` is what holds that.
async fn labels_of(provider: &GmailProvider, key: &ProviderKey) -> Vec<String> {
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a snapshot");
    };
    objects
        .iter()
        .find(|message| message.id.key() == key)
        .expect("the snapshot carries the message")
        .mailboxes
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

/// Sends a self-addressed throwaway and returns its key.
async fn send_probe(provider: &GmailProvider, marker: &str) -> ProviderKey {
    let message_id = MessageIdHeader::new(format!("gmail-report-{marker}@example.test")).unwrap();
    let draft = Draft::new(
        message_id,
        EmailAddress::new(SELF_ADDRESS),
        vec![EmailAddress::new(SELF_ADDRESS)],
        format!("Live report probe {marker}"),
        "Report probe body.",
    );
    provider
        .submit_email(&account(), &draft)
        .await
        .expect("send the probe")
        .email_key
}

#[tokio::test]
async fn adding_spam_files_the_message_and_removing_it_alone_would_not_bring_it_back() {
    let Some(token) = token() else {
        eprintln!("skipping live Gmail report test: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // Gmail has no phishing verdict, and the capability must say so — a host that
    // offered one would get a hard 400 from the service.
    let controls = provider
        .connection_info()
        .capabilities
        .mail_report()
        .expect("Gmail advertises reporting");
    assert!(controls.verdicts.junk && controls.verdicts.not_junk);
    assert!(
        !controls.verdicts.phishing,
        "Gmail has no phishing label; offering the verdict would 400"
    );
    assert_eq!(controls.evidence, ReportEvidence::Convention);

    let marker = format!("p{}", std::process::id());
    let key = send_probe(&provider, &marker).await;

    // --- report junk -------------------------------------------------------------
    provider
        .report_message(
            &account(),
            &MessageReport::new(
                key.clone(),
                ReportVerdict::Junk,
                MailboxId::try_from("SPAM").unwrap(),
            ),
        )
        .await
        .expect("report junk");

    // The report filed the message out of the Inbox and into Junk, in one call — the
    // server drops INBOX itself, so this is the whole observable effect.
    let labels = labels_of(&provider, &key).await;
    assert!(labels.contains(&"SPAM".to_owned()), "{labels:?}");
    assert!(!labels.contains(&"INBOX".to_owned()), "{labels:?}");

    // --- report not junk ---------------------------------------------------------
    provider
        .report_message(
            &account(),
            &MessageReport::new(
                key.clone(),
                ReportVerdict::NotJunk,
                MailboxId::try_from("INBOX").unwrap(),
            ),
        )
        .await
        .expect("report not junk");

    // And the inverse brings it back — which is what makes the assertion above evidence
    // rather than a message that merely failed to sync. A bare `removeLabelIds:["SPAM"]`
    // would leave it archived, in no place label at all.
    let labels = labels_of(&provider, &key).await;
    assert!(!labels.contains(&"SPAM".to_owned()), "{labels:?}");
    assert!(
        labels.contains(&"INBOX".to_owned()),
        "not-junk must re-file the message, not merely unlabel it: {labels:?}"
    );

    // Cleanup: permanently delete the throwaway.
    if let Err(err) = provider
        .edit_mail(&account(), &MailEdit::delete(key.clone()))
        .await
    {
        eprintln!("cleanup delete gave up (leaving throwaway): {err}");
    }
}
