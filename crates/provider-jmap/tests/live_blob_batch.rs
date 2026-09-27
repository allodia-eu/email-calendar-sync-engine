//! Gated live integration: sources fetched through `Blob/get` (RFC 9404) are byte for byte
//! what a download of each returns, and a blob the server does not hold fails alone.
//!
//! The offline suite replays canned bytes whatever it is sent, so it cannot show that a
//! server accepts the call or that `data:asBase64` decodes to the delivered bytes. Equality
//! with the download, which has its own live coverage, is the claim. The control arm asks for
//! a blob that does not exist, so "every body arrived" is shown to be the server answering
//! for each id rather than an adapter that fell back to downloading them all.
//!
//! It also prints both paths' rates, which against the local harness say little about a real
//! server: the round trip here is sub-millisecond, and that is what a batch saves.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --test live_blob_batch -- --nocapture
//! ```

use std::{collections::HashMap, time::Instant};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, BlobId},
    mail::Message,
    sync::SyncUpdate,
};
use engine_provider::Provider;
use futures_util::{StreamExt, stream};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

#[tokio::test]
async fn live_a_blob_get_batch_matches_downloading_each_message() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_blob_batch: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");
    let info = provider.connection_info();
    assert!(
        info.sources_per_request > 1,
        "Stalwart's mail account advertises the blob extension",
    );

    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    let messages: Vec<Message> = objects
        .into_iter()
        .filter(|m| m.blob_id.is_some())
        .collect();
    assert!(messages.len() > 1, "the harness holds several messages");

    let started = Instant::now();
    let provider_ref = &provider;
    let alone: Vec<_> = stream::iter(&messages)
        .map(|message| async move {
            let raw = provider_ref
                .fetch_message_source(&account(), message)
                .await
                .expect("a single download");
            (message.id.key().clone(), raw.as_bytes().to_vec())
        })
        .buffered(info.concurrent_fetches)
        .collect()
        .await;
    let downloads = started.elapsed().as_secs_f64();
    let alone: HashMap<_, _> = alone.into_iter().collect();

    let started = Instant::now();
    let mut seen = vec![0usize; messages.len()];
    for (chunk_at, chunk) in messages.chunks(info.sources_per_request).enumerate() {
        let account = account();
        let mut batch = provider.fetch_message_sources(&account, chunk);
        while let Some((index, result)) = batch.next().await {
            let message = &chunk[index];
            let raw = result.unwrap_or_else(|err| panic!("{}: {err}", message.id.key()));
            assert_eq!(
                raw.as_bytes(),
                alone[message.id.key()].as_slice(),
                "Blob/get's bytes for {} differ from the download's",
                message.id.key(),
            );
            seen[chunk_at * info.sources_per_request + index] += 1;
        }
    }
    let batched = started.elapsed().as_secs_f64();
    assert!(
        seen.iter().all(|&count| count == 1),
        "every message answered once: {seen:?}",
    );
    println!(
        "{} messages: downloads {downloads:.2}s at {} in flight, Blob/get {batched:.2}s serially \
         at {} per call",
        messages.len(),
        info.concurrent_fetches,
        info.sources_per_request,
    );

    let mut gone = messages[0].clone();
    gone.blob_id = Some(BlobId::try_from("blob-this-server-never-issued").unwrap());
    let asked = [gone, messages[0].clone()];
    let account = account();
    let mut outcomes: Vec<_> = provider
        .fetch_message_sources(&account, &asked)
        .map(|(index, result)| (index, result.map(|_| ()).map_err(|err| err.class())))
        .collect()
        .await;
    outcomes.sort_by_key(|(index, _)| *index);
    assert_eq!(outcomes[1], (1, Ok(())));
    assert!(
        matches!(outcomes[0], (0, Err(FailureClass::Permanent))),
        "a blob the server does not hold fails alone: {outcomes:?}",
    );
}
