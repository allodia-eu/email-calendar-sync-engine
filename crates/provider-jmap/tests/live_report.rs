//! Gated live integration: reporting a message against the Stalwart harness.
//!
//! What the offline tests cannot prove is that the *request shape* is one the server
//! accepts — the fake executor answers canned bytes whatever it is sent. So this drives
//! the real `Email/set` and reads the message back.
//!
//! Two directions are asserted, deliberately. Recording "the keyword persisted" alone
//! would pass just as well against an adapter that sent nothing and a server that
//! ignored it, so each report is followed by its inverse and the *change* is what is
//! pinned. Every assertion is on harness-controlled state (keywords, membership),
//! never on the server-assigned ids.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use engine_core::{
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::{Keyword, MailboxRole, Message, SystemKeyword},
    sync::SyncUpdate,
};
use engine_provider::{MessageReport, Provider, ReportEvidence, ReportVerdict, ReportingProvider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
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

async fn messages(provider: &JmapProvider) -> Vec<Message> {
    let emails = provider.sync_email(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("expected snapshot");
    };
    objects
}

/// The id of the mailbox holding `role`.
async fn mailbox_with_role(provider: &JmapProvider, role: &MailboxRole) -> MailboxId {
    let boxes = provider.sync_mailboxes(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = boxes.update else {
        panic!("expected snapshot");
    };
    objects
        .into_iter()
        .find(|mailbox: &engine_core::mail::Mailbox| mailbox.role.as_ref() == Some(role))
        .unwrap_or_else(|| panic!("no mailbox with role {role:?}"))
        .id
}

/// Re-reads `key` and returns its keywords plus mailbox membership.
async fn state_of(provider: &JmapProvider, key: &MessageId) -> (Vec<Keyword>, Vec<MailboxId>) {
    let message = messages(provider)
        .await
        .into_iter()
        .find(|m| &m.id == key)
        .expect("message still present");
    (
        message.keywords.iter().cloned().collect(),
        message.mailboxes.iter().cloned().collect(),
    )
}

/// The write-side key for a synced message.
fn target_key(id: &MessageId) -> ProviderKey {
    ProviderKey::new(id.as_str()).expect("a synced id is a valid provider key")
}

#[tokio::test]
async fn a_junk_report_sets_the_keyword_and_files_the_message_and_the_inverse_undoes_it() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live report test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    // The session must advertise the capability at all — and say it is a convention,
    // because JMAP gives a client no way to learn whether the server trained.
    let controls = provider
        .connection_info()
        .capabilities
        .mail_report()
        .expect("a writable JMAP account advertises reporting");
    assert!(controls.verdicts.junk && controls.verdicts.not_junk && controls.verdicts.phishing);
    assert_eq!(controls.evidence, ReportEvidence::Convention);

    let inbox = mailbox_with_role(&provider, &MailboxRole::Inbox).await;
    let junk = mailbox_with_role(&provider, &MailboxRole::Junk).await;

    // Pick a message that currently sits in the Inbox and is not already reported.
    let target = messages(&provider)
        .await
        .into_iter()
        .find(|m| {
            m.mailboxes.contains(&inbox)
                && !m.keywords.contains(&Keyword::system(SystemKeyword::Junk))
        })
        .expect("an inbox message to report")
        .id;
    let (before_keywords, before_mailboxes) = state_of(&provider, &target).await;
    assert!(before_mailboxes.contains(&inbox));

    // --- report junk -------------------------------------------------------------
    let receipt = provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::Junk, junk.clone()),
        )
        .await
        .expect("report junk");
    // A JMAP id is account-global and survives the move.
    assert_eq!(receipt.message_key, target_key(&target));

    let (keywords, mailboxes) = state_of(&provider, &target).await;
    assert!(
        keywords.contains(&Keyword::system(SystemKeyword::Junk)),
        "the server stored $junk: {keywords:?}"
    );
    assert!(
        mailboxes.contains(&junk) && !mailboxes.contains(&inbox),
        "the same set filed the message into Junk: {mailboxes:?}"
    );

    // --- and the inverse ---------------------------------------------------------
    // This half is what makes the first half evidence rather than a coincidence: if the
    // adapter were sending nothing, this would not move the state back either.
    provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::NotJunk, inbox.clone()),
        )
        .await
        .expect("report not junk");

    let (keywords, mailboxes) = state_of(&provider, &target).await;
    assert!(
        keywords.contains(&Keyword::system(SystemKeyword::NotJunk)),
        "$notjunk set: {keywords:?}"
    );
    assert!(
        !keywords.contains(&Keyword::system(SystemKeyword::Junk)),
        "the contradicting $junk was cleared in the same patch: {keywords:?}"
    );
    assert!(
        mailboxes.contains(&inbox),
        "not-junk files back to the Inbox: {mailboxes:?}"
    );

    // Leave the harness as it was found: the seed is shared with every other suite.
    provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::NotJunk, inbox.clone()),
        )
        .await
        .expect("restore");
    let _ = before_keywords;
}

#[tokio::test]
async fn phishing_is_a_keyword_of_its_own_not_an_alias_for_junk() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live phishing report test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = connect(&harness).await;

    let inbox = mailbox_with_role(&provider, &MailboxRole::Inbox).await;
    let junk = mailbox_with_role(&provider, &MailboxRole::Junk).await;
    let target = messages(&provider)
        .await
        .into_iter()
        .find(|m| m.mailboxes.contains(&inbox))
        .expect("an inbox message")
        .id;

    // Assert the *transition*, not the end state. A previous run of this test used to
    // leave `$phishing` set — the restore only cleared `$junk` — so the assertion below
    // passed against an adapter that sent no keyword at all.
    let (before, _) = state_of(&provider, &target).await;
    assert!(
        !before.contains(&Keyword::system(SystemKeyword::Phishing)),
        "the message must start clean or this proves nothing: {before:?}"
    );

    provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::Phishing, junk.clone()),
        )
        .await
        .expect("report phishing");

    let (keywords, _mailboxes) = state_of(&provider, &target).await;
    assert!(
        keywords.contains(&Keyword::system(SystemKeyword::Phishing)),
        "the server stored $phishing: {keywords:?}"
    );
    assert!(
        !keywords.contains(&Keyword::system(SystemKeyword::Junk)),
        "reporting phishing must not silently also assert $junk: {keywords:?}"
    );

    // Restore.
    provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::NotJunk, inbox),
        )
        .await
        .expect("restore");
}
