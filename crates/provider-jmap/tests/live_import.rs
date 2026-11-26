//! Gated live integration: importing into a JMAP account (`MailImporter` for `JmapProvider`)
//! against the Stalwart harness.
//!
//! The offline suite asserts the requests sent and reads captured answers; only the server says
//! it takes `Blob/upload` with `#b<n>` references from `Email/import`, a blob uploaded to the
//! `uploadUrl` and referenced by id, keywords, `receivedAt`, and several mailboxes per message.
//!
//! The claims every importer makes are the shared conformance check
//! (`engine_provider::import::conformance`), the same one the IMAP suite runs. This file adds
//! what is JMAP's own: one message in two mailboxes, one too large to go inline, which travels
//! by the upload URL, and a message the server refuses answered alone, beside one it stores in
//! the same request. Every claim is read back through `stream_email` from the import cursor.
//!
//! It runs as the scratch account `carol@`, because Alice's folders are count-asserted
//! elsewhere, and deletes the folders it creates whether it passes or not. The large message is
//! 7.6 MB, which counts against Stalwart's upload quota for an hour. As shipped that quota is
//! 50 MB per account (`jmap.md`), about six runs an hour; the harness raises it.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --all-features --test live_import -- --nocapture
//! ```

#![cfg(feature = "import")]

use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, resume_unwind},
    time::{SystemTime, UNIX_EPOCH},
};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::{Keyword, Message, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    sync::SyncUpdate,
};
use engine_provider::{
    MailboxEdit, MailboxWrites, Provider,
    import::{MessageImport, conformance},
};
use futures_util::FutureExt;
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

async fn folder(provider: &JmapProvider, name: String) -> MailboxId {
    provider
        .edit_mailbox(&account(), &MailboxEdit::create(name, None))
        .await
        .expect("create a scratch folder")
        .mailbox
        .expect("the create names its folder")
}

/// A message of about `body` bytes, unique to `run`.
fn large(subject: &str, run: u128, body: usize) -> RawMime {
    let line = format!("Imported whole, run {run}, line padding to the width of a mail line.\r\n");
    let mut text = format!(
        "From: Sender <sender@example.com>\r\n\
         To: Receiver <receiver@example.com>\r\n\
         Subject: {subject}\r\n\
         Message-ID: <import-{subject}-{run}@example.com>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n"
    );
    while text.len() < body {
        text.push_str(&line);
    }
    RawMime::new(text)
}

#[tokio::test]
async fn live_an_import_keeps_what_every_importer_keeps_and_what_jmap_adds() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_import: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let (user, password) = harness.scratch[1].auth();
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(user, password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect");
    let run = nanos();

    let first = folder(&provider, format!("Imp€rt {run}")).await;
    let second = folder(&provider, format!("Import second {run}")).await;
    let checked = AssertUnwindSafe(import_and_read_back(&provider, &first, &second, run))
        .catch_unwind()
        .await;
    for target in [first, second] {
        provider
            .edit_mailbox(&account(), &MailboxEdit::Delete { target })
            .await
            .expect("delete a scratch folder");
    }
    if let Err(failed) = checked {
        resume_unwind(failed);
    }
}

async fn import_and_read_back(
    provider: &JmapProvider,
    first: &MailboxId,
    second: &MailboxId,
    run: u128,
) {
    let importer = provider
        .mail_importer()
        .expect("a writable mail account imports");
    let limits = importer.mail_import_limits(&account()).await.unwrap();
    eprintln!("stated limits {limits:?}");
    assert_eq!(limits.max_mailboxes().get(), 100);
    assert_eq!(
        limits.max_message_size().map(std::num::NonZeroU64::get),
        Some(50_000_000)
    );

    // The claims every importer makes, read back through the account-wide cursor.
    let receipts =
        conformance::stores_as_sent("stalwart-jmap", importer, provider, &account(), first, run)
            .await;
    assert_eq!(
        receipts.dated.kept.keywords, receipts.asked,
        "JMAP stores every keyword it is given"
    );

    // What is JMAP's own, in one batch.
    let cursor = importer.import_cursor(&account(), first).await.unwrap();
    let custom: BTreeSet<Keyword> = ["$seen", "custom-tag"]
        .into_iter()
        .map(|name| Keyword::new(name).unwrap())
        .collect();
    let both = MessageImport::new(
        conformance::raw_message("both", run),
        Memberships::new([first.clone(), second.clone()]).unwrap(),
    )
    .with_metadata(MessageMetadata::default().with_keywords(custom.clone()));
    // Larger than one `Blob/upload` takes (`maxSizeBlobSet`, 7,499,488): by the upload URL.
    let big = MessageImport::new(
        large("big", run, 7_600_000),
        Memberships::of_one(first.clone()),
    );
    let nowhere = MessageImport::new(
        conformance::raw_message("nowhere", run),
        Memberships::of_one(MailboxId::try_from("zzzzzzz").unwrap()),
    );
    let too_many = MessageImport::new(
        conformance::raw_message("too-many", run),
        Memberships::new((0..101).map(|n| MailboxId::try_from(format!("m{n}").as_str()).unwrap()))
            .unwrap(),
    );
    let batch = [both, big, nowhere, too_many];
    let answers = importer.import_messages(&account(), &batch).await;

    assert_eq!(answers.len(), 4);
    let both_answer = answers[0].as_ref().expect("two mailboxes");
    let big_answer = answers[1].as_ref().expect("a message by the upload URL");
    let refused = answers[2]
        .as_ref()
        .expect_err("a mailbox that does not exist");
    eprintln!("a missing mailbox: {refused}");
    assert_eq!(refused.class(), FailureClass::InvalidState);
    assert_eq!(
        refused.target_refusal(),
        Some(engine_provider::TargetRefusal::NoSuchContainer)
    );
    assert!(
        answers[3]
            .as_ref()
            .expect_err("over maxMailboxesPerEmail")
            .is_item_unavailable()
    );
    assert_eq!(both_answer.kept.keywords, custom);

    let scope = provider
        .sync_email(&account(), Some(&cursor))
        .await
        .expect("delta from the import cursor");
    let SyncUpdate::Delta { changed, .. } = scope.update else {
        panic!("a pass from the import cursor is a delta");
    };
    assert_eq!(changed.len(), 2, "exactly the two stored: {changed:?}");
    let find = |key: Option<&engine_core::ids::ProviderKey>| -> Message {
        let key = key.expect("JMAP names the key");
        changed
            .iter()
            .find(|message| message.id.key() == key)
            .cloned()
            .expect("listed")
    };
    let both_back = find(both_answer.key.as_ref());
    let big_back = find(big_answer.key.as_ref());

    let mailboxes: BTreeSet<&MailboxId> = both_back.mailboxes.iter().collect();
    assert_eq!(
        mailboxes,
        BTreeSet::from([first, second]),
        "in both mailboxes"
    );
    let kept: BTreeSet<Keyword> = both_back.keywords.intersection(&custom).cloned().collect();
    assert_eq!(kept, custom, "a custom keyword is stored");

    for (back, sent) in [(&both_back, &batch[0]), (&big_back, &batch[1])] {
        let source = provider
            .fetch_message_source(&account(), back)
            .await
            .expect("source");
        assert_eq!(source.as_bytes(), sent.raw.as_bytes(), "byte for byte");
    }
}
