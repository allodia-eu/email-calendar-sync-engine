//! Gated live check that a stored draft's keyword is kept as a hidden Gmail label on the message
//! under the draft, read back as the keyword over the API and over IMAP, labelled again after a
//! save that replaces that message, and absent after a save that does not ask for it.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set; run it as `live_drafts.rs` describes. The IMAP leg
//! signs in with the same token (`XOAUTH2`), so it needs the `https://mail.google.com/` scope
//! `tools/google-oauth` asks for by default. Nothing is sent: the draft is addressed to the test
//! account itself and deleted at the end, and so is the label, whose name carries the run's own
//! suffix.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, Keyword, Message, MessageMetadata},
    sync::{SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    Credential, CredentialSource, CredentialSubject, Draft, FixedCredential, KeywordName,
    MailboxEdit, MailboxWrites, Provider,
};
use futures_util::StreamExt;
use provider_google::{GmailProvider, GoogleClient};
use provider_imap::{ImapAccount, ImapConfig};

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
    Keyword::new("engine-draft-probe").unwrap()
}

/// A day back, so a snapshot reads only recent mail and stays inside the per-user quota.
fn yesterday() -> CalendarDate {
    let day = time::OffsetDateTime::now_utc().date() - time::Duration::days(1);
    CalendarDate::new(day.year(), u8::from(day.month()), day.day()).unwrap()
}

/// The account's one source, which both its API and its IMAP half ask.
fn source(token: &str) -> Arc<dyn CredentialSource> {
    Arc::new(FixedCredential::new(
        CredentialSubject::new("live"),
        Credential::bearer_for(SELF_ADDRESS.to_owned(), token.to_owned()),
    ))
}

fn provider(token: &str, names: Vec<KeywordName>) -> GmailProvider {
    let client = GoogleClient::connect_with_source(
        source(token),
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GmailProvider::new(client)
        .with_since(yesterday())
        .with_keyword_names(names)
}

/// `provider`, reading mail over IMAP as a host that connects one does.
async fn over_imap(token: &str, provider: GmailProvider) -> GmailProvider {
    let config = ImapConfig::from_credential_source(
        "imap.gmail.com:993".to_owned(),
        "imap.gmail.com".to_owned(),
        source(token),
    );
    let account = ImapAccount::connect(&config, engine_tls::TlsClientConfig::bundled().connector())
        .await
        .expect("the token must open an IMAP session");
    provider
        .with_imap(
            account
                .gmail()
                .expect("imap.gmail.com advertises X-GM-EXT-1"),
        )
        .unwrap_or_else(|err| panic!("one credential subject: {:?}", err.class()))
}

fn draft(unique: u128, body: &str) -> Draft {
    let me = EmailAddress::new(SELF_ADDRESS);
    Draft::new(
        MessageIdHeader::new(format!("gmail-draft-label-{unique}@allodia-e2e.test")).unwrap(),
        me.clone(),
        vec![me],
        subject(unique),
        body,
    )
}

/// By subject, because Gmail rewrites a draft's `Message-ID` on the way in.
fn subject(unique: u128) -> String {
    format!("provider-google draft label probe {unique}")
}

/// The draft titled for this run, as a pass reads it, waiting for a pass to show it.
async fn streamed(provider: &GmailProvider, unique: u128) -> Message {
    let account = account();
    for _ in 0..10 {
        let mut chunks =
            provider.stream_email(&account, None, SyncWindow::since(yesterday()).into(), 0, 0);
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.expect("a pass that completes");
            if let Some(found) = chunk
                .changed
                .into_iter()
                .find(|m| m.envelope.subject.as_deref() == Some(subject(unique).as_str()))
            {
                return found;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("the draft never reached a pass");
}

#[tokio::test]
async fn live_a_draft_keeps_its_keyword_as_a_label_on_every_save_that_asks_for_it() {
    let Some(token) = token() else {
        eprintln!("skipping live draft label test: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("Engine draft probe {unique}");
    let names = || vec![KeywordName::new(keyword(), &name).unwrap()];
    let saving = provider(&token, names());
    assert!(saving.connection_info().capabilities.draft_keywords());
    let tagged = BTreeSet::from([keyword()]);

    let first = saving
        .put_draft(
            &account(),
            &draft(unique, "v1"),
            None,
            &MessageMetadata::default().with_keywords(tagged.clone()),
        )
        .await
        .expect("first save");
    let stored = streamed(&saving, unique).await;

    // `drafts.update` replaces the message under the draft; the save labels the new one.
    let second = saving
        .put_draft(
            &account(),
            &draft(unique, "v2"),
            Some(&first.key),
            &MessageMetadata::default().with_keywords(tagged.clone()),
        )
        .await
        .expect("re-save asking for it");
    let resaved = streamed(&saving, unique).await;
    let over_imap_read =
        streamed(&over_imap(&token, provider(&token, names())).await, unique).await;

    let third = saving
        .put_draft(
            &account(),
            &draft(unique, "v3"),
            Some(&second.key),
            &MessageMetadata::default(),
        )
        .await
        .expect("re-save asking for none");
    let unlabelled = streamed(&saving, unique).await;

    let _ = saving.delete_draft(&account(), &third.key).await;
    let unnamed = provider(&token, Vec::new());
    let sync = unnamed
        .sync_mailboxes(&account(), None)
        .await
        .expect("labels");
    if let SyncUpdate::Snapshot { objects, .. } = sync.update
        && let Some(label) = objects.into_iter().find(|m| m.name == name)
    {
        let _ = unnamed
            .edit_mailbox(&account(), &MailboxEdit::Delete { target: label.id })
            .await;
    }

    assert_eq!(first.kept.keywords, tagged, "the receipt says it was kept");
    assert!(
        stored.keywords.contains(&keyword()),
        "{:?}",
        stored.keywords
    );
    assert!(
        stored.mailboxes.iter().any(|id| id.as_str() == "DRAFT"),
        "{:?}",
        stored.mailboxes
    );
    assert_eq!(second.key, first.key, "the draft keeps its id");
    assert_ne!(resaved.id, stored.id, "the message under it is a new one");
    assert_eq!(second.kept.keywords, tagged);
    assert!(
        resaved.keywords.contains(&keyword()),
        "{:?}",
        resaved.keywords
    );
    assert!(
        over_imap_read.keywords.contains(&keyword()),
        "read back over IMAP: {:?} in {:?}",
        over_imap_read.keywords,
        over_imap_read.mailboxes
    );
    // Both directions: a save that does not ask for it leaves a draft without it.
    assert_eq!(third.kept.keywords, BTreeSet::new());
    assert!(
        !unlabelled.keywords.contains(&keyword()),
        "{:?}",
        unlabelled.keywords
    );
}
