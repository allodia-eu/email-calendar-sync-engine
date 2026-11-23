//! Gated live integration: a draft's importance against a real Gmail account.
//!
//! A Gmail draft is the assembled MIME, so what only the API answers is that the
//! `Importance` and `X-Priority` headers survive `drafts.create` and come back from a
//! `format=metadata` read, which returns only the headers `metadataHeaders` names. The
//! normal draft is the control: it carries neither header and reads back normal.
//! Nothing is sent.
//!
//! Gmail rewrites the `Message-ID` on `drafts.create` (`google.md`), so a draft is found by
//! its subject, unique to the run.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set, so `cargo test --workspace` stays green:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_importance -- --nocapture
//! ```

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, Importance, Message, MessageMetadata},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_google::{GmailProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The test account's own address; nothing here is ever sent.
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
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GmailProvider::new(client)
}

/// One whole-account snapshot, taken once: a Gmail snapshot fetches every message, and
/// several in quick succession exhaust the per-user query quota (`live_drafts.rs`).
async fn snapshot(provider: &GmailProvider) -> Vec<Message> {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Delta {
        changed: objects, ..
    } = emails.update
    else {
        panic!("a cursorless sync is an additive backfill");
    };
    objects
}

#[tokio::test]
async fn live_gmail_a_drafts_importance_reads_back() {
    let Some(token) = token() else {
        eprintln!("skipping live_gmail_a_drafts_importance: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let cases = [Importance::High, Importance::Low, Importance::Normal];

    let mut keys = Vec::new();
    for importance in cases {
        let draft = Draft::new(
            MessageIdHeader::new(format!("live-gmail-importance-{run}@test.local")).unwrap(),
            EmailAddress::new(SELF_ADDRESS),
            vec![EmailAddress::new(SELF_ADDRESS)],
            format!("Importance probe {} {run}", importance.as_str()),
            "Saved as a draft by the live test (never sent).",
        )
        .with_importance(importance);
        keys.push(
            provider
                .put_draft(&account(), &draft, None, &MessageMetadata::default())
                .await
                .map(|stored| stored.key)
                .expect("save"),
        );
    }

    let messages = snapshot(&provider).await;
    for importance in cases {
        let subject = format!("Importance probe {} {run}", importance.as_str());
        let stored = messages
            .iter()
            .find(|m| m.envelope.subject.as_deref() == Some(subject.as_str()))
            .expect("the saved draft syncs back");
        assert_eq!(stored.envelope.importance, importance, "{subject}");
    }

    for key in keys {
        provider
            .delete_draft(&account(), &key)
            .await
            .expect("discard");
    }
}
