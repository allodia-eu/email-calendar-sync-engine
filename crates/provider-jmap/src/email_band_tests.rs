//! The band walk's request shape and its stored position, against `BoundedServer`, whose query
//! honours `anchor` as a server does. It ignores the filter, so every message it holds is in the
//! band here, and what these assert about the filter is what was **sent**.

use std::sync::Arc;

use engine_core::{
    ids::ProviderKey,
    sync::{MailboxWindows, SyncState, SyncWindow, Widening},
    time::CalendarDate,
};
use engine_provider::{EmailChunk, PassMode, Provider};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{Position, carried, join, page_chunks, split};
use crate::{
    JmapProvider,
    provider::{
        bounded_server::{BoundedServer, email},
        provider_test_support::account,
    },
};

const LIMIT: usize = 500;

fn widening() -> Widening {
    let since = |month| SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap());
    Widening::new(since(4).into(), since(1).into()).unwrap()
}

fn mailbox_of(n: usize) -> Arc<BoundedServer> {
    let server = Arc::new(BoundedServer::new(LIMIT));
    server.seed("Email", (0..n).map(email));
    server
}

fn provider_over(server: &Arc<BoundedServer>) -> JmapProvider {
    JmapProvider::with_executor(Box::new(server.clone()))
}

/// Every `Email/query` the server was sent, by its arguments.
fn queries(server: &BoundedServer) -> Vec<Value> {
    server
        .requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| request["methodCalls"].as_array().unwrap().clone())
        .filter(|call| call[0] == "Email/query")
        .map(|call| call[1].clone())
        .collect()
}

fn keys(chunks: &[EmailChunk]) -> Vec<String> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.changed.iter())
        .map(|message| message.id.key().as_str().to_owned())
        .collect()
}

async fn band(provider: &JmapProvider, cursor: &SyncState, take: usize) -> Vec<EmailChunk> {
    walk(provider, cursor, &widening(), take).await
}

/// The band continued, as the caller says when the widening resumes.
async fn resumed(provider: &JmapProvider, cursor: &SyncState, take: usize) -> Vec<EmailChunk> {
    walk(provider, cursor, &widening().resuming(), take).await
}

async fn walk(
    provider: &JmapProvider,
    cursor: &SyncState,
    widening: &Widening,
    take: usize,
) -> Vec<EmailChunk> {
    provider
        .widen_email(&account(), cursor, widening, LIMIT, 0)
        .take(take)
        .map(Result::unwrap)
        .collect()
        .await
}

#[tokio::test]
async fn a_band_walks_its_filter_keeps_the_state_and_checkpoints_each_page() {
    let server = mailbox_of(1203);
    let provider = provider_over(&server);
    assert!(
        provider
            .connection_info()
            .capabilities
            .widens_mail_by_band()
    );
    let kept = SyncState::new("7");

    let chunks = band(&provider, &kept, usize::MAX).await;

    let filter = json!({ "after": "2026-01-01T00:00:00Z", "before": "2026-04-01T00:00:00Z" });
    let sent = queries(&server);
    assert!(
        sent.iter().all(|query| query["filter"] == filter),
        "{sent:?}"
    );
    assert_eq!(keys(&chunks).len(), 1203);
    assert!(chunks.iter().all(|chunk| chunk.mode == PassMode::Additive));
    assert!(chunks.iter().all(|chunk| chunk.present.is_empty()));
    let checkpoints: Vec<&SyncState> = chunks
        .iter()
        .filter_map(|c| c.advance_to.as_ref())
        .collect();
    assert_eq!(checkpoints.len(), 3, "one a page");
    let (state, position) = split(checkpoints[0]);
    assert_eq!(state, kept, "the band moved the state");
    let position = position.expect("a page records where the walk is");
    assert_eq!(position.read.last().unwrap().as_str(), "x00499");
    assert_eq!(checkpoints[2], &kept, "the band ends on the state it kept");
}

#[tokio::test]
async fn a_killed_band_resumes_after_its_stored_anchor() {
    let server = mailbox_of(1203);
    let provider = provider_over(&server);
    let first = band(&provider, &SyncState::new("7"), 1).await;
    let resume_from = first[0].advance_to.clone().unwrap();
    let before = queries(&server).len();

    let rest = resumed(&provider, &resume_from, usize::MAX).await;

    let resumed = &queries(&server)[before];
    assert_eq!(resumed["anchor"], "x00499", "{resumed}");
    assert_eq!(resumed["anchorOffset"], 1);
    let (mut all, rest) = (keys(&first), keys(&rest));
    assert_eq!(rest.len(), 703, "the band walked again: {}", rest.len());
    all.extend(rest);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 1203);
}

#[tokio::test]
async fn a_destroyed_anchor_steps_back_and_mail_inserted_in_the_gap_is_stored() {
    let server = mailbox_of(1203);
    let provider = provider_over(&server);
    let first = band(&provider, &SyncState::new("7"), 1).await;
    let resume_from = first[0].advance_to.clone().unwrap();
    server.change("Email", |store| {
        store.destroy("x00499");
        let mut gap = email(0);
        gap["id"] = json!("x00499a");
        store.upsert(gap);
    });

    let rest = resumed(&provider, &resume_from, usize::MAX).await;

    let mut all = keys(&first);
    all.extend(keys(&rest));
    assert!(all.contains(&"x00499a".to_owned()), "the gap's message");
    assert!(all.contains(&"x01202".to_owned()));
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        1204,
        "every band message, the destroyed one read before"
    );
}

#[tokio::test]
async fn a_position_the_caller_does_not_resume_is_ignored() {
    let server = mailbox_of(10);
    let provider = provider_over(&server);
    let stale = Position {
        read: vec![ProviderKey::new("x00004").unwrap()],
    };
    let chunks = band(&provider, &join(&SyncState::new("7"), &stale), usize::MAX).await;
    assert_eq!(
        queries(&server)[0]["position"],
        0,
        "a position the caller did not ask to continue was continued"
    );
    assert_eq!(keys(&chunks).len(), 10);
}
#[tokio::test]
async fn a_delta_carries_the_position_and_a_snapshot_drops_it() {
    let server = mailbox_of(3);
    let provider = provider_over(&server);
    let position = Position {
        read: vec![ProviderKey::new("x00001").unwrap()],
    };
    let windows = MailboxWindows::default();
    let last = |chunks: Vec<engine_provider::ProviderResult<EmailChunk>>| {
        chunks.into_iter().map(Result::unwrap).next_back().unwrap()
    };

    let delta = last(
        provider
            .stream_email(
                &account(),
                Some(&join(&SyncState::new("3"), &position)),
                windows.clone(),
                50,
                0,
            )
            .collect()
            .await,
    );
    assert_eq!(delta.mode, PassMode::Additive);
    assert_eq!(
        delta.advance_to,
        Some(carried(SyncState::new("3"), Some(&position)))
    );

    // A state the server can no longer calculate changes from: a snapshot, which reconciles.
    let snapshot = last(
        provider
            .stream_email(
                &account(),
                Some(&join(&SyncState::new("99"), &position)),
                windows,
                50,
                0,
            )
            .collect()
            .await,
    );
    assert!(snapshot.is_reconcile_final());
    assert_eq!(snapshot.advance_to, Some(SyncState::new("3")));
}

#[test]
fn a_cursor_that_only_looks_like_a_band_is_a_plain_state() {
    for raw in ["band:", "band:x1", "7"] {
        let state = SyncState::new(raw);
        assert_eq!(split(&state), (state.clone(), None), "{raw}");
    }
    let position = Position { read: Vec::new() };
    let joined = join(&SyncState::new("7"), &position);
    assert_eq!(split(&joined), (SyncState::new("7"), Some(position)));
    assert_eq!(carried(SyncState::new("8"), None), SyncState::new("8"));
}

#[test]
fn a_page_with_nothing_new_still_checkpoints_where_the_walk_is() {
    let chunks = page_chunks(Vec::new(), Some(4), 0, SyncState::new("here"));
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].changed, []);
    assert_eq!(chunks[0].mode, PassMode::Additive);
    assert_eq!(chunks[0].advance_to, Some(SyncState::new("here")));
}
