//! Offline tests for [`fetch_batch`], over connections a scripted dial hands out in order.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use engine_core::{
    error::FailureClass,
    ids::{MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
};
use futures_util::StreamExt;

use super::fetch_batch;
use crate::{
    mock::{MockStream, Recorded, script, written},
    pool::{Dial, ImapPool},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";

/// A pool whose dials serve `scripts` in order, one per connection, and the bytes the client
/// wrote on each.
fn pool_over(scripts: Vec<String>) -> (Arc<ImapPool<MockStream>>, Arc<Mutex<Vec<Recorded>>>) {
    let queue = Arc::new(Mutex::new(VecDeque::from(scripts)));
    let recordings = Arc::new(Mutex::new(Vec::new()));
    let sessions = Arc::clone(&recordings);
    let dial: Dial<MockStream> = Arc::new(move || {
        let next = queue.lock().unwrap().pop_front().unwrap_or_default();
        let (stream, recorded) = MockStream::new(script(&[GREETING, &next]));
        sessions.lock().unwrap().push(recorded);
        Box::pin(async move { Connection::open(stream).await })
    });
    (ImapPool::new(dial, 5, Duration::from_hours(1)), recordings)
}

fn message(key: &str) -> Message {
    let mailbox = key.rsplit_once('@').unwrap().1;
    Message::new(
        MessageId::try_from(key).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    )
}

fn examine(tag: &str, validity: u32) -> String {
    format!("* 3 EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n{tag} OK [READ-ONLY] done\r\n")
}

fn body(uid: u32, text: &str) -> String {
    format!(
        "* {uid} FETCH (UID {uid} BODY[] {{{}}}\r\n{text})\r\n",
        text.len()
    )
}

/// Every `(index, outcome)` the batch yields, in the order it yielded them.
async fn run(
    pool: &Arc<ImapPool<MockStream>>,
    messages: &[Message],
) -> Vec<(usize, Result<Vec<u8>, FailureClass>)> {
    fetch_batch(pool, messages)
        .map(|(index, result)| {
            (
                index,
                result
                    .map(|raw| raw.as_bytes().to_vec())
                    .map_err(|err| err.class()),
            )
        })
        .collect()
        .await
}

fn sent(recordings: &Mutex<Vec<Recorded>>) -> String {
    recordings.lock().unwrap().iter().map(written).collect()
}

#[tokio::test]
async fn one_request_brings_back_every_body_of_a_mailbox() {
    let server = [
        examine("a1", 7),
        body(42, "first"),
        body(43, "second"),
        body(45, "third"),
        "a2 OK FETCH completed\r\n".to_owned(),
    ]
    .concat();
    let (pool, recordings) = pool_over(vec![server]);
    let messages = [
        message("imap:v7:u42@INBOX"),
        message("imap:v7:u43@INBOX"),
        message("imap:v7:u45@INBOX"),
    ];

    let outcomes = run(&pool, &messages).await;

    assert_eq!(
        outcomes,
        vec![
            (0, Ok(b"first".to_vec())),
            (1, Ok(b"second".to_vec())),
            (2, Ok(b"third".to_vec())),
        ]
    );
    let sent = sent(&recordings);
    assert_eq!(sent.matches("EXAMINE").count(), 1, "{sent}");
    assert!(
        sent.contains("a2 UID FETCH 42:43,45 (BODY.PEEK[])"),
        "{sent}"
    );
}

#[tokio::test]
async fn each_mailbox_is_its_own_request() {
    let server = [
        examine("a1", 7),
        body(42, "inbox"),
        "a2 OK FETCH completed\r\n".to_owned(),
        examine("a3", 9),
        body(5, "sent"),
        "a4 OK FETCH completed\r\n".to_owned(),
    ]
    .concat();
    let (pool, recordings) = pool_over(vec![server]);
    let messages = [message("imap:v7:u42@INBOX"), message("imap:v9:u5@Sent")];

    let outcomes = run(&pool, &messages).await;

    assert_eq!(
        outcomes,
        vec![(0, Ok(b"inbox".to_vec())), (1, Ok(b"sent".to_vec()))]
    );
    assert!(sent(&recordings).contains("a4 UID FETCH 5 (BODY.PEEK[])"));
}

#[tokio::test]
async fn a_stale_key_and_an_expunged_uid_fail_alone() {
    // UID 40 was keyed under a UIDVALIDITY the mailbox no longer has, so it is not asked for
    // at all; UID 43 was expunged, so the server answers for 42 only.
    let server = [
        examine("a1", 7),
        body(42, "kept"),
        "a2 OK FETCH completed\r\n".to_owned(),
    ]
    .concat();
    let (pool, recordings) = pool_over(vec![server]);
    let messages = [
        message("imap:v6:u40@INBOX"),
        message("imap:v7:u42@INBOX"),
        message("imap:v7:u43@INBOX"),
    ];

    let mut outcomes = run(&pool, &messages).await;
    outcomes.sort_by_key(|(index, _)| *index);

    assert_eq!(
        outcomes,
        vec![
            (0, Err(FailureClass::Conflict)),
            (1, Ok(b"kept".to_vec())),
            (2, Err(FailureClass::Conflict)),
        ]
    );
    assert!(sent(&recordings).contains("UID FETCH 42:43 "));
}

#[tokio::test]
async fn a_connection_lost_mid_batch_asks_again_for_the_rest_only() {
    // The first connection delivers one body and then closes. The retry must not ask for the
    // body it already has: that would download it twice and hand it on twice.
    let cut = [examine("a1", 7), body(42, "first")].concat();
    let retry = [
        examine("a1", 7),
        body(43, "second"),
        "a2 OK FETCH completed\r\n".to_owned(),
    ]
    .concat();
    let (pool, recordings) = pool_over(vec![cut, retry]);
    let messages = [message("imap:v7:u42@INBOX"), message("imap:v7:u43@INBOX")];

    let outcomes = run(&pool, &messages).await;

    assert_eq!(
        outcomes,
        vec![(0, Ok(b"first".to_vec())), (1, Ok(b"second".to_vec()))]
    );
    let second = written(&recordings.lock().unwrap()[1]);
    assert!(second.contains("UID FETCH 43 (BODY.PEEK[])"), "{second}");
}

#[tokio::test]
async fn a_batch_that_loses_its_connection_twice_reports_the_rest_as_lost() {
    let cut = examine("a1", 7);
    let (pool, _recordings) = pool_over(vec![cut.clone(), cut]);
    let messages = [message("imap:v7:u42@INBOX"), message("imap:v7:u43@INBOX")];

    let outcomes = run(&pool, &messages).await;

    assert_eq!(
        outcomes,
        vec![
            (0, Err(FailureClass::Retryable)),
            (1, Err(FailureClass::Retryable))
        ]
    );
}

#[tokio::test]
async fn an_unparseable_key_is_refused_before_the_wire() {
    let (pool, recordings) = pool_over(vec![]);
    let messages = [message("imap:v7:u42@INBOX\r\na9 DELETE INBOX")];

    let outcomes = run(&pool, &messages).await;

    assert_eq!(outcomes, vec![(0, Err(FailureClass::InvalidState))]);
    assert!(sent(&recordings).is_empty());
}
