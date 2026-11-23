//! Gated live check that a stored draft's keyword is kept as an Outlook category and read back
//! as the keyword, that a rewrite in place keeps it, and that a rewrite no longer asking for it
//! takes it off while leaving a category the saving device has no name for.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set; run it as `live_drafts.rs` describes. Nothing is
//! sent: the draft is addressed to the test account itself and deleted at the end. The category
//! names carry the run's own suffix, so a run never finds an earlier run's category, and a
//! category set only on a message leaves nothing behind once it is gone.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, MessageMetadata},
    sync::SyncUpdate,
};
use engine_provider::{Draft, KeywordName, MailEdit, Provider};
use provider_graph::{GraphClient, GraphProvider};

const SELF_ADDRESS: &str = "allodia-e2e@outlook.com";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn keyword() -> Keyword {
    Keyword::new("engine-draft-probe").unwrap()
}

/// A keyword only the "person" device has a name for: to the saving device its category is
/// the person's own.
fn own() -> Keyword {
    Keyword::new("engine-draft-own").unwrap()
}

fn provider(token: &str, names: Vec<KeywordName>) -> GraphProvider {
    let client = GraphClient::connect(
        token.to_owned(),
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("drafts").unwrap()).with_keyword_names(names)
}

fn draft(message_id: &MessageIdHeader, body: &str) -> Draft {
    let me = EmailAddress::new(SELF_ADDRESS);
    Draft::new(
        message_id.clone(),
        me.clone(),
        vec![me],
        "provider-graph draft category probe",
        body,
    )
}

/// The keywords the stored draft `key` carries, as a sync reads them.
async fn keywords(provider: &GraphProvider, key: &ProviderKey) -> BTreeSet<Keyword> {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.id.key() == key)
        .expect("the saved draft syncs back")
        .keywords
}

#[tokio::test]
async fn live_a_draft_keeps_its_keyword_as_a_category_across_a_re_save_and_drops_it_on_request() {
    let Some(token) = token() else {
        eprintln!("skipping live draft category test: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let probe_name = KeywordName::new(keyword(), format!("Engine draft probe {unique}")).unwrap();
    let own_name = KeywordName::new(own(), format!("Engine own {unique}")).unwrap();
    let saving = provider(&token, vec![probe_name.clone()]);
    let person = provider(&token, vec![own_name.clone()]);
    let reader = provider(&token, vec![probe_name, own_name]);
    assert!(saving.connection_info().capabilities.draft_keywords());
    let message_id =
        MessageIdHeader::new(format!("graph-draft-cat-{unique}@allodia-e2e.test")).unwrap();
    let tagged = BTreeSet::from([keyword()]);

    let first = saving
        .put_draft(
            &account(),
            &draft(&message_id, "v1"),
            None,
            &MessageMetadata::default().with_keywords(tagged.clone()),
        )
        .await
        .expect("first save");
    let stored = keywords(&reader, &first.key).await;

    person
        .edit_mail(
            &account(),
            &MailEdit::SetKeywords {
                target: first.key.clone(),
                add: [own()].into(),
                remove: BTreeSet::new(),
            },
        )
        .await
        .expect("the person's own category");

    let second = saving
        .put_draft(
            &account(),
            &draft(&message_id, "v2"),
            Some(&first.key),
            &MessageMetadata::default().with_keywords(tagged.clone()),
        )
        .await
        .expect("re-save asking for it");
    let after_second = keywords(&reader, &second.key).await;
    let third = saving
        .put_draft(
            &account(),
            &draft(&message_id, "v3"),
            Some(&second.key),
            &MessageMetadata::default(),
        )
        .await
        .expect("re-save asking for none");
    let after_third = keywords(&reader, &third.key).await;

    saving
        .delete_draft(&account(), &third.key)
        .await
        .expect("discard");

    assert_eq!(first.kept.keywords, tagged, "the receipt says it was kept");
    assert!(stored.contains(&keyword()), "{stored:?}");
    assert_eq!(
        second.key, first.key,
        "a text edit rewrites the draft in place"
    );
    assert_eq!(second.kept.keywords, tagged);
    assert!(after_second.contains(&keyword()), "{after_second:?}");
    assert!(after_second.contains(&own()), "{after_second:?}");
    // Both directions: the keyword is gone once a save stops asking for it, and the category
    // the saving device has no name for is left alone.
    assert_eq!(third.kept.keywords, BTreeSet::new());
    assert!(!after_third.contains(&keyword()), "{after_third:?}");
    assert!(after_third.contains(&own()), "{after_third:?}");
}
