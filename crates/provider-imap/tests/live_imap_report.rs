//! Gated live integration: reporting a message, against **every** configured IMAP
//! server — Stalwart (IMAP4rev2) and both Dovecot dialects.
//!
//! The offline suite replays a scripted transcript, so it proves the parsing and says
//! nothing about whether a real server accepts `UID STORE +FLAGS ($Junk)` — a keyword
//! is exactly the kind of thing a server may answer `OK` to and then discard. Running
//! against two independent implementations is what makes "the keyword persisted"
//! evidence rather than one server's habit.
//!
//! Each report is followed by its inverse and the **change** is asserted, so a passing
//! run cannot be produced by an adapter that sends nothing against a message that was
//! already flagged.
//!
//! **Each test owns one seeded message in the dedicated `Reported` mailbox**, addressed
//! by its `Message-ID`. Both harnesses seed it, and the messages are kept out of INBOX:
//! a report stores a keyword and may move the message, and INBOX is count-asserted
//! elsewhere. Owning one *each* matters too — cargo runs a binary's tests concurrently
//! and CI passes no `--test-threads=1`, so two tests sharing a message read each other's
//! keywords, which is a failure that only ever appears on the runner.
//!
//! Skips per server when its address variable is unset.

#[path = "common/imap_live.rs"]
mod imap_live;

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Keyword, Message, SystemKeyword},
    sync::SyncUpdate,
};
use engine_provider::{MessageReport, Provider, ReportVerdict};
use imap_live::{LiveProvider, SERVERS, connect_to};

/// The mailbox holding this suite's own messages (seeded by both harnesses).
const REPORTED: &str = "Reported";

/// The junk/not-junk test's own message (`docker/stalwart/seed/mail/10-report-junk.eml`).
const JUNK_MESSAGE_ID: &str = "report-junk-0001@test.local";
/// The keyword-permission test's own message (`…/11-report-phishing.eml`).
const PERMISSION_MESSAGE_ID: &str = "report-phishing-0001@test.local";

fn account() -> AccountId {
    AccountId::try_from("live-harness").unwrap()
}

fn home() -> MailboxId {
    MailboxId::try_from(REPORTED).unwrap()
}

async fn reported_messages(provider: &LiveProvider) -> Vec<Message> {
    match provider.sync_email(&account(), None).await {
        Ok(scope) => match scope.update {
            SyncUpdate::Snapshot { objects, .. } => objects,
            SyncUpdate::Delta { .. } => panic!("a first pass is a snapshot"),
        },
        Err(err) => panic!("sync_email failed: {err}"),
    }
}

/// This test's own seeded message, by `Message-ID` — never by position, which is how one
/// message ends up shared by two concurrently running tests.
async fn my_message(provider: &LiveProvider, message_id: &str, label: &str) -> ProviderKey {
    let id = reported_messages(provider)
        .await
        .into_iter()
        .find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .unwrap_or_else(|| {
            panic!(
                "{label} has no seeded {message_id} in {REPORTED} — is its harness seed current?"
            )
        })
        .id;
    ProviderKey::new(id.as_str()).expect("a synced id is a provider key")
}

/// The keywords currently on `key`, re-read from the server.
async fn keywords_of(provider: &LiveProvider, key: &ProviderKey) -> Vec<Keyword> {
    reported_messages(provider)
        .await
        .into_iter()
        .find(|m| m.id.as_str() == key.as_str())
        .map(|m| m.keywords.iter().cloned().collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_junk_report_stores_the_registered_keyword_and_the_inverse_clears_it() {
    let test = "a_junk_report_stores_the_registered_keyword_and_the_inverse_clears_it";
    for server in &SERVERS {
        let Some(provider) = connect_to(server, REPORTED, test).await else {
            continue;
        };

        // Every configured server must advertise reporting; the verdict set is
        // unconditional on IMAP because all three keywords are just keywords.
        let controls = provider
            .connection_info()
            .capabilities
            .mail_report()
            .unwrap_or_else(|| panic!("{} advertises reporting", server.label));
        assert!(controls.verdicts.junk && controls.verdicts.not_junk && controls.verdicts.phishing);

        let key = my_message(&provider, JUNK_MESSAGE_ID, server.label).await;

        // The precondition, asserted rather than assumed: a message that already carried
        // $Junk would satisfy the assertion below against an adapter that sent nothing.
        let before = keywords_of(&provider, &key).await;
        assert!(
            !before.contains(&Keyword::system(SystemKeyword::Junk)),
            "{} must start unreported or this proves nothing: {before:?}",
            server.label
        );

        // Report into the message's own mailbox so nothing moves: this test is about the
        // keyword, and a move would change the UID and confuse the re-read.
        provider
            .report_message(
                &account(),
                &MessageReport::new(key.clone(), ReportVerdict::Junk, home()),
            )
            .await
            .unwrap_or_else(|err| panic!("report junk on {}: {err}", server.label));

        let after = keywords_of(&provider, &key).await;
        assert!(
            after.contains(&Keyword::system(SystemKeyword::Junk)),
            "{} stored $Junk: {after:?}",
            server.label
        );

        // The inverse — which is what makes the assertion above mean something.
        provider
            .report_message(
                &account(),
                &MessageReport::new(key.clone(), ReportVerdict::NotJunk, home()),
            )
            .await
            .unwrap_or_else(|err| panic!("report not junk on {}: {err}", server.label));

        let after = keywords_of(&provider, &key).await;
        assert!(
            after.contains(&Keyword::system(SystemKeyword::NotJunk)),
            "{} stored $NotJunk: {after:?}",
            server.label
        );
        assert!(
            !after.contains(&Keyword::system(SystemKeyword::Junk)),
            "{} cleared the contradicting $Junk: {after:?}",
            server.label
        );
    }
}

#[tokio::test]
async fn every_configured_server_permits_new_keywords() {
    // The adapter refuses a report when `PERMANENTFLAGS` lacks `\*`, because the STORE
    // would otherwise be accepted and discarded. That refusal branch has **no server to
    // exercise it**: Stalwart and both Dovecot dialects advertise `\*`. This test pins
    // the premise instead — if a harness bump ever changed it, the refusal would start
    // firing on every report and this names why.
    let test = "every_configured_server_permits_new_keywords";
    for server in &SERVERS {
        let Some(provider) = connect_to(server, REPORTED, test).await else {
            continue;
        };
        let key = my_message(&provider, PERMISSION_MESSAGE_ID, server.label).await;

        // This asserts the *premise*, not the adapter: the report only gets past the
        // `PERMANENTFLAGS` guard while the server still advertises `\*`. It would pass
        // against an adapter that skipped the check entirely — the offline suite is what
        // proves the check exists and refuses.
        provider
            .report_message(
                &account(),
                &MessageReport::new(key, ReportVerdict::NotJunk, home()),
            )
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "{} refused a report — does it still advertise \\* in PERMANENTFLAGS? {err}",
                    server.label
                )
            });
    }
}
