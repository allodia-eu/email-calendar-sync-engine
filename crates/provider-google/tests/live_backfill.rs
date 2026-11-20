//! Gated live proofs of the Gmail backfill: a first sync is a backfill, then a list-only
//! reconcile, then deltas; and a backfill stopped after a page resumes at Gmail's own page
//! token, the second pass fetching what the first had not and nothing it had.
//!
//! The test mailbox holds about 190 messages, two pages of a hundred, so the first page's
//! checkpoint carries a real `nextPageToken`. The first pass is dropped at that checkpoint,
//! exactly where a suspended app's pass ends, and the second starts from it.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_backfill -- --nocapture
//! ```

use std::collections::BTreeSet;

use engine_core::{
    ids::AccountId,
    sync::{SyncState, SyncUpdate, SyncWindow},
};
use engine_provider::{EmailChunk, Provider};
use futures_util::StreamExt;
use provider_google::{GmailProvider, GoogleClient};

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GmailProvider::new(client)
}

fn fetched(chunks: &[EmailChunk]) -> BTreeSet<String> {
    chunks
        .iter()
        .flat_map(|chunk| &chunk.changed)
        .map(|message| message.id.key().as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn live_a_backfill_stopped_after_a_page_resumes_at_its_token() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_backfill_stopped_after_a_page_resumes_at_its_token: \
             GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = provider(token);
    let account = account();

    // The first pass, dropped at its first checkpoint.
    let mut first = Vec::new();
    let mut stream = provider.stream_email(&account, None, SyncWindow::full(), 0, 0);
    let checkpoint: SyncState = loop {
        let chunk = stream
            .next()
            .await
            .expect("the backfill reaches a checkpoint")
            .expect("a chunk");
        if let Some(reached) = chunk.advance_to.clone() {
            first.push(chunk);
            break reached;
        }
        first.push(chunk);
    };
    drop(stream);
    println!("stopped at {}", checkpoint.as_str());
    assert!(
        checkpoint.as_str().starts_with("backfill;h=") && !checkpoint.as_str().ends_with(";p="),
        "the first checkpoint names the next page: {}",
        checkpoint.as_str()
    );

    // The second pass, from that checkpoint, to the end of the window.
    let rest: Vec<EmailChunk> = provider
        .stream_email(&account, Some(&checkpoint), SyncWindow::full(), 0, 0)
        .map(|chunk| chunk.expect("a chunk"))
        .collect()
        .await;

    let (before, after) = (fetched(&first), fetched(&rest));
    println!(
        "{} fetched before the stop, {} after",
        before.len(),
        after.len()
    );
    assert!(!after.is_empty(), "the resumed pass fetched nothing");
    assert!(
        before.is_disjoint(&after),
        "the resumed pass fetched again what the first had: {:?}",
        before.intersection(&after).collect::<Vec<_>>()
    );
    let last = rest
        .iter()
        .rev()
        .find_map(|chunk| chunk.advance_to.as_ref())
        .expect("the resumed pass checkpoints");
    assert!(
        last.as_str().starts_with("reconcile;h="),
        "{}",
        last.as_str()
    );
}

#[tokio::test]
async fn live_backfill_reconcile_then_delta_cycle() {
    let Some(token) = token() else {
        eprintln!("skipping live_backfill_reconcile_then_delta_cycle: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // A first sync is an additive backfill, ending on a reconcile cursor.
    let backfill = provider
        .sync_email(&account(), None)
        .await
        .expect("backfill");
    let SyncUpdate::Delta { changed, .. } = &backfill.update else {
        panic!("a first sync is an additive backfill");
    };
    assert!(!changed.is_empty(), "the test account has messages");
    // Every backfilled message carries a provider-assigned thread and an envelope.
    assert!(changed.iter().all(|m| m.thread.is_some()));
    assert!(backfill.next_cursor.as_str().starts_with("reconcile;h="));

    // The pass after it lists the window's ids, fetches none, and ends on the bare historyId.
    let reconcile = provider
        .sync_email(&account(), Some(&backfill.next_cursor))
        .await
        .expect("reconcile");
    let SyncUpdate::Snapshot { objects, present } = &reconcile.update else {
        panic!("the pass after a backfill reconciles");
    };
    assert!(objects.is_empty(), "a reconcile fetches no message");
    assert!(
        changed.iter().all(|m| present.contains(m.id.key())),
        "the reconcile would tombstone a message the backfill fetched"
    );
    let cursor = reconcile.next_cursor.clone();
    assert!(cursor.as_str().chars().all(|c| c.is_ascii_digit()));

    // An immediate delta from that cursor must not error and must advance (or hold) the
    // cursor: proof that Gmail accepts the real history.list request shape.
    let delta = provider
        .sync_email(&account(), Some(&cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot(), "a delta from a live cursor");
    let _ = &delta.next_cursor;
}
