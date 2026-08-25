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
//! **Each test owns one seeded message in the dedicated `Reported` mailbox**, addressed
//! by its Message-ID. Two reasons, and both are failures that only appear on the runner:
//! a report *files* the message, and a JMAP move replaces an Email's `mailboxIds`, so
//! reporting a shared fixture would drag it out of the count-asserted INBOX; and cargo
//! runs a binary's tests concurrently while CI passes no `--test-threads=1`, so two
//! tests sharing one message read each other's keywords.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use engine_core::{
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::{Keyword, MailboxRole, Message, SystemKeyword},
    sync::SyncUpdate,
};
use engine_provider::{MessageReport, Provider, ReportEvidence, ReportVerdict};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

/// The mailbox holding this suite's own messages (`docker/stalwart/seed.sh`).
const REPORTED: &str = "Reported";

/// The junk/not-junk test's own message (`seed/mail/10-report-junk.eml`).
const JUNK_MESSAGE_ID: &str = "report-junk-0001@test.local";
/// The phishing test's own message (`seed/mail/11-report-phishing.eml`).
const PHISHING_MESSAGE_ID: &str = "report-phishing-0001@test.local";

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
    mailboxes(provider)
        .await
        .into_iter()
        .find(|mailbox| mailbox.role.as_ref() == Some(role))
        .unwrap_or_else(|| panic!("no mailbox with role {role:?}"))
        .id
}

/// The id of the seeded mailbox named `name` — `Reported` has no role, so it is found by
/// name rather than by role.
async fn mailbox_named(provider: &JmapProvider, name: &str) -> MailboxId {
    mailboxes(provider)
        .await
        .into_iter()
        .find(|mailbox| mailbox.name == name)
        .unwrap_or_else(|| panic!("no mailbox named {name} — is the harness seed current?"))
        .id
}

async fn mailboxes(provider: &JmapProvider) -> Vec<engine_core::mail::Mailbox> {
    let boxes = provider.sync_mailboxes(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = boxes.update else {
        panic!("expected snapshot");
    };
    objects
}

/// This test's own seeded message, by `Message-ID`.
///
/// By identity rather than by position: a positional pick silently follows whatever the
/// seed happens to order first, which is how one message ends up shared by two tests.
async fn my_message(provider: &JmapProvider, message_id: &str) -> MessageId {
    messages(provider)
        .await
        .into_iter()
        .find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .unwrap_or_else(|| panic!("the seeded {message_id} message — is the harness seed current?"))
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

    let home = mailbox_named(&provider, REPORTED).await;
    let junk = mailbox_with_role(&provider, &MailboxRole::Junk).await;
    let target = my_message(&provider, JUNK_MESSAGE_ID).await;

    // The precondition, asserted rather than assumed: a message that already carried
    // $junk would satisfy every assertion below against an adapter that sent nothing.
    let (before, before_mailboxes) = state_of(&provider, &target).await;
    assert!(
        !before.contains(&Keyword::system(SystemKeyword::Junk)),
        "the message must start unreported or this proves nothing: {before:?}"
    );
    assert!(before_mailboxes.contains(&home));

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
        mailboxes.contains(&junk) && !mailboxes.contains(&home),
        "the same set filed the message into Junk: {mailboxes:?}"
    );

    // --- and the inverse ---------------------------------------------------------
    // This half is what makes the first half evidence rather than a coincidence: if the
    // adapter were sending nothing, this would not move the state back either.
    provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::NotJunk, home.clone()),
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
        mailboxes.contains(&home),
        "not-junk files to the destination it was given: {mailboxes:?}"
    );
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

    let home = mailbox_named(&provider, REPORTED).await;
    let junk = mailbox_with_role(&provider, &MailboxRole::Junk).await;
    let target = my_message(&provider, PHISHING_MESSAGE_ID).await;

    // Assert the *transition*, not the end state. A message that already carried
    // `$phishing` would pass every assertion below against an adapter sending no keyword
    // at all — which is what a shared message and a concurrent junk test produced.
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

    // Restore, so a re-run of this test starts from the precondition it asserts.
    provider
        .report_message(
            &account(),
            &MessageReport::new(target_key(&target), ReportVerdict::NotJunk, home),
        )
        .await
        .expect("restore");
    let (after, _) = state_of(&provider, &target).await;
    assert!(
        !after.contains(&Keyword::system(SystemKeyword::Phishing)),
        "not-junk clears the accusation it contradicts: {after:?}"
    );
}
