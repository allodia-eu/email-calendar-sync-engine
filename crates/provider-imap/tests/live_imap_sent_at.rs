//! Gated live integration: a message read over IMAP says it was sent when its `Date` header
//! does, and was received when the server stored it, against the Stalwart harness.
//!
//! The message is planted over JMAP with `sentAt` and `receivedAt` a month apart, which Stalwart
//! writes as the `Date` header and reports as the IMAP `INTERNALDATE`, in the scratch account
//! `bob@`. A Sent copy imported or filed after the fact has exactly this shape. The `Date` carries
//! a `+0100` offset, so the read must normalise it to UTC as JMAP's `sentAt` is.
//!
//! Skips with no `STALWART_HTTP_ADDR`.

use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
    sync::SyncUpdate,
};
use engine_provider::Provider;
use provider_imap::{Credentials, ImapAccount, ImapConfig};
use stalwart_harness::Harness;

/// The token every subject this suite plants carries, so a rerun can find its residue.
const MARKER: &str = "sentatimap";

/// Destroys what this run planted, however the run ends.
struct Planted<'a> {
    harness: &'a Harness,
    auth: (&'a str, &'a str),
    ids: Vec<String>,
}

impl Drop for Planted<'_> {
    fn drop(&mut self) {
        let _ = self.harness.destroy_emails_as(self.auth, &self.ids);
    }
}

#[tokio::test]
async fn a_message_is_sent_when_its_date_header_says_and_received_when_it_was_stored() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping a_message_is_sent_when_its_date_header_says_...: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let scratch = harness.scratch[0].clone();
    let auth = scratch.auth();
    let residue = harness
        .emails_with_subject_as(auth, MARKER)
        .expect("find residue");
    harness
        .destroy_emails_as(auth, &residue)
        .expect("clear residue of an earlier run");
    let subject = format!("{MARKER} filed a month later");
    let mut planted = Planted {
        harness: &harness,
        auth,
        ids: Vec::new(),
    };
    planted.ids.push(
        harness
            .plant_email_sent_as(
                auth,
                "sent",
                &subject,
                "2026-10-29T10:00:00+01:00",
                "2026-11-20T20:15:44Z",
            )
            .expect("plant a message"),
    );

    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        Credentials::password(auth.0, auth.1),
    );
    let imap = ImapAccount::connect(
        &config,
        engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
    )
    .await
    .expect("connect IMAP");
    let account = AccountId::try_from("live-sent-at-imap").unwrap();
    let SyncUpdate::Snapshot { objects, .. } = imap
        .provider(MailboxId::try_from("INBOX").unwrap())
        .sync_mailboxes(&account, None)
        .await
        .expect("folders")
        .update
    else {
        panic!("a folder list is a snapshot");
    };
    let sent = objects
        .into_iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Sent))
        .expect("a Sent folder")
        .id;

    let SyncUpdate::Snapshot { objects, .. } = imap
        .provider(sent)
        .sync_email(&account, None)
        .await
        .expect("sync Sent")
        .update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    let message = objects
        .into_iter()
        .find(|message| message.envelope.subject.as_deref() == Some(subject.as_str()))
        .expect("the planted message");
    assert_eq!(
        message.sent_at.map(|at| at.to_string()).as_deref(),
        Some("2026-10-29T09:00:00Z"),
        "the Date header, normalised to UTC"
    );
    assert_eq!(
        message.received_at.map(|at| at.to_string()).as_deref(),
        Some("2026-11-20T20:15:44Z"),
        "the INTERNALDATE"
    );
}
