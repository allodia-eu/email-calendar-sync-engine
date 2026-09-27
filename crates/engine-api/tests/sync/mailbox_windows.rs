//! One mailbox held further back than its account, and read over a span of time, through the
//! facade a host drives.

use engine_api::{CalendarDate, Engine, MailboxWindows, UtcDateTime};

use super::*;

fn since(year: i32, month: u8, day: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, day).unwrap())
}

fn sent() -> MailboxId {
    MailboxId::try_from("s").unwrap()
}

/// An Inbox message and three Sent messages, spread over three years.
fn provider() -> FakeProvider {
    let dated = |id: &str, mailbox: &str, received: &str| {
        let mut message = message(id, mailbox, id);
        message.received_at = Some(received.parse().unwrap());
        message
    };
    FakeProvider {
        mailboxes: vec![
            mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("s", "Sent", Some(MailboxRole::Sent)),
        ],
        messages: vec![
            dated("in-old", "a", "2025-02-01T09:00:00Z"),
            dated("sent-new", "s", "2026-05-01T09:00:00Z"),
            dated("sent-old", "s", "2025-02-01T09:00:00Z"),
            dated("sent-older", "s", "2024-02-01T09:00:00Z"),
        ],
        ..FakeProvider::new()
    }
}

async fn keys(engine: &Engine) -> Vec<String> {
    let mut keys: Vec<String> = engine
        .messages(&account())
        .await
        .unwrap()
        .into_iter()
        .map(|message| message.id.key().as_str().to_owned())
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn a_depth_reduction_prune_keeps_what_a_deeper_mailbox_holds() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(
            core::slice::from_ref(&provider()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;

    // The account narrows to this year; Sent is held back to the start of last year.
    let windows = MailboxWindows::new(since(2026, 1, 1)).deepen(sent(), since(2025, 1, 1));
    let report = engine
        .prune_account_mail_outside_window(&account(), windows)
        .await
        .unwrap();

    assert_eq!(report.messages_removed, 2, "in-old and sent-older");
    assert_eq!(keys(&engine).await, vec!["sent-new", "sent-old"]);
}

#[tokio::test]
async fn a_mailbox_span_returns_its_messages_newest_first_and_how_far_back_it_reaches() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(
            core::slice::from_ref(&provider()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    let at = |text: &str| text.parse::<UtcDateTime>().unwrap();

    let span = engine
        .mail_in_mailbox_between(
            &account(),
            &sent(),
            at("2024-06-01T00:00:00Z"),
            at("2027-01-01T00:00:00Z"),
            10,
        )
        .await
        .unwrap();
    let keys: Vec<&str> = span
        .messages
        .iter()
        .map(|message| message.id.key().as_str())
        .collect();
    assert_eq!(
        keys,
        vec!["sent-new", "sent-old"],
        "Sent only, newest first"
    );
    assert!(span.messages.iter().all(|m| m.mailboxes.contains(&sent())));
    assert_eq!(
        span.oldest,
        Some(at("2024-02-01T09:00:00Z")),
        "the oldest the mailbox holds, outside the span asked for"
    );

    let newest = engine
        .mail_in_mailbox_between(
            &account(),
            &sent(),
            at("2020-01-01T00:00:00Z"),
            at("2027-01-01T00:00:00Z"),
            1,
        )
        .await
        .unwrap();
    assert_eq!(newest.messages.len(), 1);
    assert_eq!(newest.messages[0].id.key().as_str(), "sent-new");

    let nothing = engine
        .mail_in_mailbox_between(
            &account(),
            &MailboxId::try_from("never").unwrap(),
            at("2020-01-01T00:00:00Z"),
            at("2027-01-01T00:00:00Z"),
            10,
        )
        .await
        .unwrap();
    assert!(nothing.messages.is_empty());
    assert_eq!(nothing.oldest, None);
}
