//! Gated live check that a delta pass pages at the stream's `fetch_batch`: the server accepts
//! `Prefer: odata.maxpagesize` on the first page, on every `@odata.nextLink` continuation and on
//! the deltaLink, and serves no page larger than the ask.
//!
//! A continuation that lost the header would answer Graph's own ten a page, so a batch of three
//! over an inbox of more than ten messages fails here if any request goes out without it.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set; see `live_provider.rs` for how to mint one.

mod common;

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    sync::{MailboxWindows, SyncState},
};
use engine_provider::{EmailChunk, Provider};
use futures_util::StreamExt;
use provider_graph::{GraphClient, GraphProvider};

const BATCH: usize = 3;

fn inbox(token: &str) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

/// One pass from `cursor` with `chunk_size` 0, so each chunk that carries messages is one
/// server page.
async fn pass(of: &GraphProvider, cursor: Option<&SyncState>, batch: usize) -> Vec<EmailChunk> {
    let account = common::account();
    let mut stream = of.stream_email(&account, cursor, MailboxWindows::default(), batch, 0);
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.expect("a page"));
    }
    chunks
}

fn keys(chunks: &[EmailChunk]) -> BTreeSet<String> {
    chunks
        .iter()
        .flat_map(|chunk| &chunk.changed)
        .map(|message| message.id.key().as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn live_a_delta_pass_pages_at_the_fetch_batch() {
    let Some(token) = common::token() else {
        eprintln!("skipping live_a_delta_pass_pages_at_the_fetch_batch: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = inbox(&token);

    let small = pass(&provider, None, BATCH).await;
    let whole = pass(&provider, None, 0).await;
    assert!(
        keys(&whole).len() > 10,
        "the inbox needs more than one default page of mail to tell a page size apart"
    );
    let pages: Vec<usize> = small
        .iter()
        .map(|chunk| chunk.changed.len())
        .filter(|&n| n > 0)
        .collect();
    assert!(
        pages.iter().all(|&n| n <= BATCH),
        "every page, continuations included, holds at most {BATCH}: {pages:?}"
    );
    assert_eq!(
        keys(&small),
        keys(&whole),
        "paging small reaches the same messages"
    );

    // The deltaLink accepts the header too.
    let cursor = small
        .last()
        .and_then(|chunk| chunk.advance_to.clone())
        .expect("the pass ends at a deltaLink");
    pass(&provider, Some(&cursor), BATCH).await;
}
