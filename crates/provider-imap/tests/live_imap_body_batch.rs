//! Gated live integration: a batch of message sources, fetched with one `UID FETCH <set>
//! (BODY.PEEK[])`, is byte for byte what fetching each message alone returns, against every
//! configured IMAP server.
//!
//! The offline suite replays canned bytes whatever the command, so it cannot tell a set the
//! server accepts from one it does not, nor a row order it streams from one the parser
//! assumes. The claim here is equality with the single-message path, which has its own live
//! coverage, and the control arm asks for UIDs the server does not have, so that "every body
//! arrived" is shown to be the server answering the set rather than an adapter that echoed
//! what it was asked for. Skips per server when its address variable is unset.

#[path = "common/imap_live.rs"]
mod imap_live;

use std::collections::HashMap;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MessageId},
    mail::Message,
    sync::SyncUpdate,
};
use engine_provider::Provider;
use futures_util::StreamExt;
use imap_live::{SERVERS, connect};

fn account() -> AccountId {
    AccountId::try_from("live-batch").unwrap()
}

/// Every message the INBOX snapshot holds.
async fn inbox(provider: &imap_live::LiveProvider) -> Vec<Message> {
    match provider.sync_email(&account(), None).await {
        Ok(scope) => match scope.update {
            SyncUpdate::Snapshot { objects, .. } => objects,
            SyncUpdate::Delta { .. } => panic!("a first pass is a snapshot"),
        },
        Err(err) => panic!("sync_email failed: {err}"),
    }
}

/// `message`, re-keyed to another UID in the same mailbox and UIDVALIDITY.
fn with_uid(message: &Message, uid: u32) -> Message {
    let key = message.id.key().as_str();
    let (prefix, rest) = key.split_once(":u").expect("an IMAP key");
    let mailbox = rest.split_once('@').expect("an IMAP key").1;
    let mut other = message.clone();
    other.id = MessageId::try_from(format!("{prefix}:u{uid}@{mailbox}").as_str()).unwrap();
    other
}

#[tokio::test]
async fn a_batch_of_sources_matches_fetching_each_alone() {
    for server in &SERVERS {
        let Some(provider) =
            connect(server, "a_batch_of_sources_matches_fetching_each_alone").await
        else {
            continue;
        };
        let info = provider.connection_info();
        assert!(
            info.sources_per_request > 1,
            "{}: batching is advertised",
            server.label
        );
        assert!(
            info.concurrent_fetches > 1,
            "{}: overlapping is advertised",
            server.label
        );

        let messages = inbox(&provider).await;
        assert!(
            messages.len() > 1,
            "{}: the seed holds several",
            server.label
        );

        let mut alone = HashMap::new();
        for message in &messages {
            let raw = provider
                .fetch_message_source(&account(), message)
                .await
                .unwrap_or_else(|err| panic!("{}: single fetch: {err}", server.label));
            alone.insert(message.id.key().clone(), raw.as_bytes().to_vec());
        }

        let mut seen = vec![0usize; messages.len()];
        let account = account();
        let mut batch = provider.fetch_message_sources(&account, &messages);
        while let Some((index, result)) = batch.next().await {
            seen[index] += 1;
            let raw = result.unwrap_or_else(|err| panic!("{}: batch fetch: {err}", server.label));
            assert_eq!(
                raw.as_bytes(),
                alone[messages[index].id.key()].as_slice(),
                "{}: the batch's bytes for {} differ from the single fetch's",
                server.label,
                messages[index].id.key(),
            );
        }
        assert!(
            seen.iter().all(|&count| count == 1),
            "{}: every message is answered exactly once: {seen:?}",
            server.label,
        );
    }
}

#[tokio::test]
async fn a_batch_answers_only_for_the_messages_the_server_has() {
    for server in &SERVERS {
        let Some(provider) = connect(
            server,
            "a_batch_answers_only_for_the_messages_the_server_has",
        )
        .await
        else {
            continue;
        };
        let messages = inbox(&provider).await;
        let real = messages.first().expect("a seeded message").clone();
        // UIDs far past anything the seed delivered: the server has no body to send.
        let asked = [
            with_uid(&real, 3_999_999_001),
            real.clone(),
            with_uid(&real, 3_999_999_002),
        ];

        let mut outcomes: Vec<_> = provider
            .fetch_message_sources(&account(), &asked)
            .map(|(index, result)| (index, result.map(|_| ()).map_err(|err| err.class())))
            .collect()
            .await;
        outcomes.sort_by_key(|(index, _)| *index);

        assert_eq!(
            outcomes,
            vec![
                (0, Err(FailureClass::Conflict)),
                (1, Ok(())),
                (2, Err(FailureClass::Conflict)),
            ],
            "{}",
            server.label,
        );
    }
}
