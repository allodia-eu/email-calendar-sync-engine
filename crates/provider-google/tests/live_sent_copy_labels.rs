//! Gated live check that a keyword a draft asks for on its filed copy reaches the sent message
//! as a Gmail label, that the label is read as the keyword rather than as a folder, and that a
//! second device registering the names the other way round reuses the label rather than
//! creating its own.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set; run it as `live_provider.rs` describes. Every
//! message it sends is addressed to the test account itself and deleted at the end, and so is
//! the label, whose names carry the run's own suffix.

use engine_core::{
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, Mailbox, Message},
    sync::SyncUpdate,
    time::CalendarDate,
};
use engine_provider::{Draft, KeywordName, MailEdit, MailboxEdit, MailboxWrites, Provider};
use provider_google::{GmailProvider, GoogleClient};

const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn keyword() -> Keyword {
    Keyword::new("engine-probe").unwrap()
}

fn provider(token: &str, names: Vec<KeywordName>) -> GmailProvider {
    let client = GoogleClient::connect(
        token.to_owned(),
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
        .with_since(yesterday())
        .with_keyword_names(names)
}

/// A day back, so the snapshots read only this run's mail: four whole-account snapshots in a
/// minute exceed Gmail's per-user query quota.
fn yesterday() -> CalendarDate {
    let day = time::OffsetDateTime::now_utc().date() - time::Duration::days(1);
    CalendarDate::new(day.year(), u8::from(day.month()), day.day()).unwrap()
}

fn draft(unique: u128, which: &str) -> Draft {
    let me = EmailAddress::new(SELF_ADDRESS);
    Draft::new(
        MessageIdHeader::new(format!("gmail-label-{which}-{unique}@allodia-e2e.test")).unwrap(),
        me.clone(),
        vec![me],
        format!("provider-google label probe {which} {unique}"),
        "Sent by the provider-google live label test; safe to delete.",
    )
    .with_sent_copy_keyword(keyword())
}

async fn mailboxes(provider: &GmailProvider) -> Vec<Mailbox> {
    let sync = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("labels");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected a label snapshot");
    };
    objects
}

async fn message(provider: &GmailProvider, key: &ProviderKey) -> Message {
    let sync = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected a message snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.id.key() == key)
        .expect("the sent message is in the snapshot")
}

#[tokio::test]
async fn live_the_sent_copy_carries_the_keyword_as_a_hidden_label_and_a_second_name_reuses_it() {
    let Some(token) = token() else {
        eprintln!("skipping live label test: GOOGLE_ACCESS_TOKEN unset");
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
    let first_device = provider(&token, named(&english, &dutch));
    let second_device = provider(&token, named(&dutch, &english));
    let unnamed = provider(&token, Vec::new());

    let first = first_device
        .submit_email(&account(), &draft(unique, "a"))
        .await
        .expect("send");
    assert!(
        first.sent_copy_keywords.contains(&keyword()),
        "the receipt keeps it"
    );
    let second = second_device
        .submit_email(&account(), &draft(unique, "b"))
        .await
        .expect("send");
    assert!(second.sent_copy_keywords.contains(&keyword()));

    // One label for both devices, under the first device's name.
    let ours: Vec<Mailbox> = mailboxes(&unnamed)
        .await
        .into_iter()
        .filter(|m| m.name == english || m.name == dutch)
        .collect();
    assert_eq!(ours.len(), 1, "one label, not one per language: {ours:?}");
    assert_eq!(ours[0].name, english);
    // A device that knows the names reads it as the keyword, never as a folder.
    assert!(
        mailboxes(&first_device)
            .await
            .iter()
            .all(|m| m.id != ours[0].id)
    );
    for key in [&first.email_key, &second.email_key] {
        let seen = message(&first_device, key).await;
        assert!(seen.keywords.contains(&keyword()), "{:?}", seen.keywords);
        assert!(!seen.mailboxes.contains(&ours[0].id));
        let plain = message(&unnamed, key).await;
        assert!(
            plain.mailboxes.contains(&ours[0].id),
            "without names it is a label"
        );
    }

    for key in [first.email_key, second.email_key] {
        let _ = unnamed.edit_mail(&account(), &MailEdit::delete(key)).await;
    }
    let _ = unnamed
        .edit_mailbox(
            &account(),
            &MailboxEdit::Delete {
                target: ours[0].id.clone(),
            },
        )
        .await;
}
