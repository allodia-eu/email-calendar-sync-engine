//! Offline tests for [`ImapAccount`]: that the folders it binds share one budget.
//!
//! Every account here is [`ImapAccount::offline`] — one scripted connection and a dial that
//! always fails. So a call that succeeds *proves* it ran on the account's one connection, and a
//! call that needed a second one fails the way it would with no network.

use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::Provider;

use super::*;
use crate::mock::{MockStream, Recorded, script, written};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN completed\r\n";
/// A rev1 server offering `IDLE` and nothing that needs an `ENABLE`.
const CAPABILITY_IDLE: &str = "* CAPABILITY IMAP4rev1 IDLE\r\na2 OK done\r\n";

fn mailbox(name: &str) -> MailboxId {
    MailboxId::try_from(name).unwrap()
}

fn account_id() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

/// An offline account over one connection that has logged in and negotiated, and then serves
/// `rest` to whatever the account's providers send.
async fn account_over(rest: &[&str]) -> (ImapAccount<MockStream>, Recorded) {
    let mut server = vec![GREETING, LOGIN_OK, CAPABILITY_IDLE];
    server.extend_from_slice(rest);
    let (stream, recorded) = MockStream::new(script(&server));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice", "pw").await.unwrap();
    connection.negotiate().await.unwrap();
    (ImapAccount::offline(connection, None), recorded)
}

#[tokio::test]
async fn two_folders_of_one_account_share_its_connection() {
    let (account, recorded) = account_over(&["a3 OK LIST done\r\n", "a4 OK LIST done\r\n"]).await;
    let inbox = account.provider(mailbox("INBOX"));
    let sent = account.provider(mailbox("Sent"));

    inbox.sync_mailboxes(&account_id(), None).await.unwrap();
    // The account cannot dial a second connection, so this succeeding means the Sent provider
    // borrowed the one the INBOX provider gave back — not one of its own.
    sent.sync_mailboxes(&account_id(), None)
        .await
        .expect("the second folder reused the account's connection");

    assert_eq!(written(&recorded).matches(" LIST ").count(), 2);
}

#[tokio::test]
async fn a_failed_call_does_not_hand_its_connection_to_the_next_folder() {
    // The server refuses the first LIST. The connection would serve the second one fine — but a
    // failure cannot say whether it left the session usable, so it is not offered again.
    let (account, recorded) =
        account_over(&["a3 NO LIST refused\r\n", "a4 OK LIST done\r\n"]).await;

    let refused = account
        .provider(mailbox("INBOX"))
        .sync_mailboxes(&account_id(), None)
        .await
        .unwrap_err();
    assert_eq!(refused.class(), FailureClass::InvalidState);

    let next = account
        .provider(mailbox("Sent"))
        .sync_mailboxes(&account_id(), None)
        .await
        .expect_err("the next folder had to dial, and this account cannot");
    assert_eq!(next.class(), FailureClass::Retryable);
    assert_eq!(written(&recorded).matches(" LIST ").count(), 1);
}

#[tokio::test]
async fn invalidating_the_account_drops_its_resting_connection() {
    let (account, recorded) = account_over(&["a3 OK LIST done\r\n"]).await;

    account.invalidate();
    let err = account
        .provider(mailbox("INBOX"))
        .sync_mailboxes(&account_id(), None)
        .await
        .expect_err("the resting connection was reused after the host said the network changed");

    assert_eq!(err.class(), FailureClass::Retryable);
    assert!(!written(&recorded).contains("LIST"));
}

#[tokio::test]
async fn a_watch_spends_one_connection_until_it_is_dropped() {
    let (account, recorded) =
        account_over(&["* OK [UIDVALIDITY 1] ok\r\na3 OK [READ-ONLY] EXAMINE done\r\n"]).await;
    let headroom = account.watch_headroom();
    assert_eq!(headroom, DEFAULT_MAX_CONNECTIONS - 1);

    let watcher = account
        .watch(mailbox("INBOX"), crate::DEFAULT_IDLE_KEEPALIVE)
        .await
        .expect("the watch took the account's resting connection");
    assert!(written(&recorded).contains("EXAMINE"));
    assert_eq!(account.watch_headroom(), headroom - 1);

    // Dropping is the release, because a watch task is aborted rather than stopped.
    drop(watcher);
    assert_eq!(account.watch_headroom(), headroom);
}

#[tokio::test]
async fn a_server_without_idle_refuses_the_watch_and_keeps_the_slot() {
    let (stream, _recorded) = MockStream::new(script(&[
        GREETING,
        LOGIN_OK,
        "* CAPABILITY IMAP4rev1\r\na2 OK done\r\n",
    ]));
    let mut connection = Connection::open(stream).await.unwrap();
    connection.login("alice", "pw").await.unwrap();
    connection.negotiate().await.unwrap();
    let account = ImapAccount::offline(connection, None);
    let headroom = account.watch_headroom();

    let err = account
        .watch(mailbox("INBOX"), crate::DEFAULT_IDLE_KEEPALIVE)
        .await
        .expect_err("no IDLE, no watch");

    assert_eq!(err.class(), FailureClass::InvalidState);
    assert_eq!(
        account.watch_headroom(),
        headroom,
        "a refused watch kept its slot, shrinking the budget for good",
    );
}

#[tokio::test]
async fn every_folder_advertises_what_the_first_connection_negotiated() {
    let (account, _recorded) = account_over(&[]).await;

    let capabilities = account
        .provider(mailbox("Archive"))
        .connection_info()
        .capabilities;

    assert!(capabilities.mail() && capabilities.idle());
    assert!(
        !capabilities.submission(),
        "no SMTP transport is configured, so nothing may claim it can send",
    );
}

#[tokio::test]
async fn a_pass_that_fails_mid_stream_does_not_park_its_connection() {
    use engine_core::sync::SyncWindow;
    use futures_util::StreamExt;

    let (account, recorded) =
        account_over(&["a3 NO SELECT refused\r\n", "a4 OK LIST done\r\n"]).await;
    let inbox = account.provider(mailbox("INBOX"));

    let items: Vec<_> = inbox
        .stream_email(&account_id(), None, SyncWindow::full(), 50, 0)
        .collect()
        .await;
    assert!(
        items.iter().any(Result::is_err),
        "the refused SELECT surfaces"
    );

    // The stream held its connection for the whole pass; failing, it must not hand it back.
    let next = inbox
        .sync_mailboxes(&account_id(), None)
        .await
        .expect_err("the failed pass's connection was reused");
    assert_eq!(next.class(), FailureClass::Retryable);
    assert!(!written(&recorded).contains("LIST"));
}

fn sent_draft() -> engine_provider::Draft {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    engine_provider::Draft::new(
        MessageIdHeader::new("pool-filing@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

#[tokio::test]
async fn a_send_with_no_connection_to_file_on_still_delivers_and_says_so() {
    // The network changed after the account connected, and nothing can be dialled: the mail
    // still leaves over SMTP, and the receipt carries both failed filing attempts.
    let (account, _recorded) = account_over(&[]).await;
    account.invalidate();
    let (smtp, _smtp_recorded) = MockStream::new(script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]));

    let receipt = account
        .provider(mailbox("INBOX"))
        .submit_over(smtp, &sent_draft(), None)
        .await
        .expect("a delivered send is never failed for a filing error");

    let detail = receipt
        .sent_copy
        .unfiled_detail()
        .expect("nothing could be filed");
    assert!(
        detail.contains("retry on a fresh session"),
        "the retry ran and its failure is on the receipt: {detail}",
    );
}

#[tokio::test]
async fn a_repair_with_no_connection_to_be_had_fails_rather_than_pretending() {
    let (account, _recorded) = account_over(&[]).await;
    account.invalidate();

    let err = account
        .provider(mailbox("INBOX"))
        .file_sent_copy(&account_id(), &sent_draft())
        .await
        .expect_err("nothing was filed, so the repair must not report success");

    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn an_account_and_its_folders_can_be_logged() {
    let (account, _recorded) = account_over(&[]).await;

    let logged = format!("{account:?} {:?}", account.provider(mailbox("Archive")));

    assert!(logged.contains("ImapAccount") && logged.contains("max_connections"));
    assert!(logged.contains("ImapProvider") && logged.contains("Archive"));
}
