//! Gated live check that a keyword a draft asks for on its filed copy reaches the Sent Items
//! copy as an Outlook category, reads back through sync as the keyword, and that a second
//! device registering the names in another order reuses the category already in use.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set; run it as `live_provider.rs` describes. Every
//! message it sends is addressed to the test account itself, and deleted at the end along
//! with the copy delivered to the Inbox. The category names carry the run's own suffix, so a
//! run never finds an earlier run's category, and a category set only on messages leaves
//! nothing behind once they are gone.

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, Keyword, Message},
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
    Keyword::new("engine-probe").unwrap()
}

fn provider(token: &str, folder: &str, names: Vec<KeywordName>) -> GraphProvider {
    let client = GraphClient::connect(
        token.to_owned(),
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from(folder).unwrap()).with_keyword_names(names)
}

fn draft(message_id: &MessageIdHeader) -> Draft {
    let me = EmailAddress::new(SELF_ADDRESS);
    Draft::new(
        message_id.clone(),
        me.clone(),
        vec![me],
        "provider-graph category probe",
        "Sent by the provider-graph live category test; safe to delete.",
    )
    .with_sent_copy_keyword(keyword())
}

/// The message with `message_id` in the provider's folder, synced fresh.
async fn find(provider: &GraphProvider, message_id: &MessageIdHeader) -> Option<Message> {
    let snapshot = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
        return None;
    };
    objects.into_iter().find(|m| {
        m.envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == message_id.as_str())
    })
}

async fn await_in(provider: &GraphProvider, message_id: &MessageIdHeader) -> Message {
    for _ in 0..30 {
        if let Some(found) = find(provider, message_id).await {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    panic!("{} never appeared", message_id.as_str());
}

async fn delete(provider: &GraphProvider, message: &Message) {
    let _ = provider
        .edit_mail(&account(), &MailEdit::delete(message.id.key().clone()))
        .await;
}

#[tokio::test]
async fn live_the_sent_copy_carries_the_keyword_as_a_category_and_a_second_name_reuses_it() {
    let Some(token) = token() else {
        eprintln!("skipping live category test: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let english = format!("Engine probe {unique}");
    let dutch = format!("Engine proef {unique}");
    let named = |create: &str, known: &str| {
        vec![
            KeywordName::new(keyword(), create)
                .unwrap()
                .also_known_as(known),
        ]
    };
    let first_device = provider(&token, "sentitems", named(&english, &dutch));
    let second_device = provider(&token, "sentitems", named(&dutch, &english));
    // Knows only the first device's name, so it sees the keyword only on a copy carrying that
    // category: the proof of which name the second device used.
    let english_only = provider(&token, "sentitems", named(&english, &english));
    let inbox = provider(&token, "inbox", named(&english, &dutch));

    let first = MessageIdHeader::new(format!("graph-cat-a-{unique}@allodia-e2e.test")).unwrap();
    let receipt = first_device
        .submit_email(&account(), &draft(&first))
        .await
        .expect("send");
    assert!(
        receipt.sent_copy_keywords.contains(&keyword()),
        "the receipt reports the category kept"
    );
    let first_copy = await_in(&first_device, &first).await;
    assert!(
        first_copy.keywords.contains(&keyword()),
        "sync reads the category back as the keyword: {:?}",
        first_copy.keywords
    );

    let second = MessageIdHeader::new(format!("graph-cat-b-{unique}@allodia-e2e.test")).unwrap();
    let receipt = second_device
        .submit_email(&account(), &draft(&second))
        .await
        .expect("send");
    assert!(receipt.sent_copy_keywords.contains(&keyword()));
    let second_copy = await_in(&english_only, &second).await;
    assert!(
        second_copy.keywords.contains(&keyword()),
        "the second device reused the category the first created, not its own name"
    );

    // Tidy up: both Sent copies, and the copies delivered to the Inbox.
    delete(&first_device, &first_copy).await;
    delete(&first_device, &second_copy).await;
    for id in [&first, &second] {
        if let Some(received) = find(&inbox, id).await {
            assert!(
                !received.keywords.contains(&keyword()),
                "the category is the sender's, never on what was delivered"
            );
            delete(&inbox, &received).await;
        }
    }
}
