//! Gated live check that a keyword a draft asks for on its filed copy reaches the sent message
//! as a Gmail label, that the label is read as the keyword rather than as a folder, and that a
//! second device registering the names the other way round reuses the label rather than
//! creating its own. The same read is checked over IMAP, where the label arrives by name in
//! `X-GM-LABELS` rather than by id.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set; run it as `live_provider.rs` describes. The IMAP
//! leg signs in with the same token (`XOAUTH2`), so it needs the `https://mail.google.com/`
//! scope `tools/google-oauth` asks for by default. Every message it sends is addressed to the
//! test account itself and deleted at the end, and so is the label, whose names carry the run's
//! own suffix.

use std::time::Duration;

use engine_core::{
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, Mailbox, Message},
    sync::{SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{Draft, KeywordName, MailEdit, MailboxEdit, MailboxWrites, Provider};
use futures_util::StreamExt;
use provider_google::{GmailProvider, GoogleClient};
use provider_imap::{Credentials, ImapAccount, ImapConfig};

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
        &engine_http::test_server::user_agent(),
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
    let SyncUpdate::Delta {
        changed: objects, ..
    } = sync.update
    else {
        panic!("a cursorless sync is an additive backfill");
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
        .submit_email(
            &account(),
            &draft(unique, "a"),
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("send");
    assert!(
        first.sent_copy_keywords.contains(&keyword()),
        "the receipt keeps it"
    );
    let second = second_device
        .submit_email(
            &account(),
            &draft(unique, "b"),
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
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

/// `provider`, reading mail over IMAP as a host that connects one does.
async fn over_imap(token: &str, provider: GmailProvider) -> GmailProvider {
    let config = ImapConfig::new(
        "imap.gmail.com:993".to_owned(),
        "imap.gmail.com".to_owned(),
        Credentials::oauth2(SELF_ADDRESS.to_owned(), token.to_owned()),
    );
    let account = ImapAccount::connect(&config, engine_tls::TlsClientConfig::bundled().connector())
        .await
        .expect("the token must open an IMAP session");
    provider.with_imap(
        account
            .gmail()
            .expect("imap.gmail.com advertises X-GM-EXT-1"),
    )
}

/// The message under `key` as a pass writes it, waiting for the copy to reach All Mail.
async fn streamed(provider: &GmailProvider, key: &ProviderKey) -> Message {
    let account = account();
    for _ in 0..10 {
        let mut chunks =
            provider.stream_email(&account, None, SyncWindow::since(yesterday()), 0, 0);
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.expect("a pass that completes");
            if let Some(found) = chunk.changed.into_iter().find(|m| m.id.key() == key) {
                return found;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("the sent message never reached a pass");
}

#[tokio::test]
async fn live_the_hidden_label_reads_back_over_imap_as_the_keyword_under_any_name_and_case() {
    let Some(token) = token() else {
        eprintln!("skipping live IMAP label test: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let created = format!("Engine probe imap {unique}");
    let sender = provider(&token, vec![KeywordName::new(keyword(), &created).unwrap()]);
    let sent = sender
        .submit_email(
            &account(),
            &draft(unique, "imap"),
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("send");
    assert!(sent.sent_copy_keywords.contains(&keyword()));
    let unnamed = provider(&token, Vec::new());
    let label = mailboxes(&unnamed)
        .await
        .into_iter()
        .find(|m| m.name == created)
        .expect("the label was created");

    // Another device's name for it, and the created one in another case.
    let reader = over_imap(
        &token,
        provider(
            &token,
            vec![
                KeywordName::new(keyword(), format!("Engine proef imap {unique}"))
                    .unwrap()
                    .also_known_as(created.to_uppercase()),
            ],
        ),
    )
    .await;
    let named = streamed(&reader, &sent.email_key).await;
    // Both directions: without a name the label is a folder over IMAP, so the keyword above
    // was read from `X-GM-LABELS` and is not a label IMAP left out.
    let plain = streamed(&over_imap(&token, unnamed).await, &sent.email_key).await;

    let unnamed = provider(&token, Vec::new());
    let _ = unnamed
        .edit_mail(&account(), &MailEdit::delete(sent.email_key))
        .await;
    let _ = unnamed
        .edit_mailbox(
            &account(),
            &MailboxEdit::Delete {
                target: label.id.clone(),
            },
        )
        .await;

    assert!(named.keywords.contains(&keyword()), "{:?}", named.keywords);
    assert!(
        !named.mailboxes.contains(&label.id),
        "{:?}",
        named.mailboxes
    );
    assert!(
        plain.mailboxes.contains(&label.id),
        "X-GM-LABELS names the hidden label: {:?}",
        plain.mailboxes
    );
    assert!(!plain.keywords.contains(&keyword()));
}
