//! Gated live integration: a stored draft's keywords over JMAP, against the Stalwart harness.
//!
//! What only a real server can answer: that an `Email/set` create carrying a keyword beside
//! `$draft` and `$seen` is accepted and stores it, so the draft syncs back carrying it, and that
//! a re-save asking for none stores a draft without it.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so `cargo test --workspace` stays green offline.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live-draft-keywords").unwrap()
}

fn keyword() -> Keyword {
    Keyword::new("engine-draft-probe").unwrap()
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect")
}

fn draft(message_id: &str, from: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(from),
        vec![EmailAddress::new("bob@test.local")],
        "Draft keyword probe",
        "Saved as a draft by the live test (never submitted).",
    )
}

/// The stored message `key`, as the account's own sync reports it.
async fn synced(provider: &JmapProvider, key: &ProviderKey) -> Message {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.id.key() == key)
        .expect("the saved draft syncs back")
}

#[tokio::test]
async fn live_jmap_a_draft_keeps_its_keyword_and_a_re_save_without_it_drops_it() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_draft_keywords: STALWART_HTTP_ADDR unset");
        return;
    };
    let provider = connect(&harness).await;
    assert!(provider.connection_info().capabilities.draft_keywords());
    let message_id = &format!(
        "live-jmap-draft-kw-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let tagged = BTreeSet::from([keyword()]);

    let first = provider
        .put_draft(
            &account(),
            &draft(message_id, &harness.account),
            None,
            &tagged,
        )
        .await
        .expect("first save");
    assert_eq!(first.keywords, tagged, "the receipt says it was kept");
    let stored = synced(&provider, &first.key).await;

    let second = provider
        .put_draft(
            &account(),
            &draft(message_id, &harness.account),
            Some(&first.key),
            &BTreeSet::new(),
        )
        .await
        .expect("second save");
    let resaved = synced(&provider, &second.key).await;

    provider
        .delete_draft(&account(), &second.key)
        .await
        .expect("discard");

    // Both directions: the keyword read back is the one the save stored, not one every
    // draft carries.
    assert!(
        stored.keywords.contains(&keyword()),
        "{:?}",
        stored.keywords
    );
    assert_eq!(second.keywords, BTreeSet::new());
    assert!(
        !resaved.keywords.contains(&keyword()),
        "{:?}",
        resaved.keywords
    );
}
