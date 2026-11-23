//! [`Provider::widen_email`]: the reopen default every adapter starts from, the capability that
//! says an adapter does better, and the `Box<dyn Provider>` forwarding that must not lose it.

use std::sync::Mutex;

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    mail::Mailbox,
    sync::{MailboxWindows, SyncState, SyncWindow, Widening},
    time::CalendarDate,
};
use futures_util::StreamExt;

use super::*;
use crate::{CalendarWrites, tests::account};

/// Records what `stream_email` was asked, and answers with one marker chunk.
#[derive(Default)]
struct Recording {
    asked: Mutex<Vec<(Option<String>, MailboxWindows)>>,
    bands: bool,
}

#[async_trait]
impl Provider for Recording {
    fn connection_info(&self) -> ConnectionInfo {
        let caps = Capabilities::none().with_mail();
        ConnectionInfo::new(if self.bands {
            caps.with_band_widening()
        } else {
            caps
        })
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        unreachable!("not synced here")
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: MailboxWindows,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        self.asked
            .lock()
            .unwrap()
            .push((cursor.map(|c| c.as_str().to_owned()), window));
        let chunk =
            EmailChunk::reconcile_last(Vec::new(), Vec::new(), None, SyncState::new("reopened"));
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

/// An adapter with a band of its own.
struct Banding;

#[async_trait]
impl Provider for Banding {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_band_widening())
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        unreachable!("not synced here")
    }

    fn widen_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: &'a SyncState,
        _widening: &'a Widening,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let chunk = EmailChunk::additive(Vec::new(), Vec::new(), Some(0), cursor.clone());
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

impl CalendarSync for Recording {}
impl crate::MailboxWrites for Recording {}
impl CalendarWrites for Recording {}
impl CalendarSync for Banding {}
impl crate::MailboxWrites for Banding {}
impl CalendarWrites for Banding {}

fn widening() -> Widening {
    let since = |month| SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap());
    Widening::new(since(4).into(), since(1).into()).unwrap()
}

#[tokio::test]
async fn the_default_reopens_a_fresh_pass_over_the_wider_window() {
    let provider = Recording::default();
    let widening = widening();
    let cursor = SyncState::new("delta-7");
    let chunks: Vec<_> = provider
        .widen_email(&account(), &cursor, &widening, 50, 0)
        .collect()
        .await;

    assert_eq!(
        *provider.asked.lock().unwrap(),
        vec![(None, widening.to().clone())],
        "a reopen starts with no cursor, under the window being widened to"
    );
    let chunk = chunks.into_iter().next().unwrap().unwrap();
    assert_eq!(chunk.mode, PassMode::Reconcile);
    assert_eq!(chunk.advance_to.unwrap().as_str(), "reopened");
    assert!(
        !provider
            .connection_info()
            .capabilities
            .widens_mail_by_band()
    );
}

#[tokio::test]
async fn a_boxed_provider_forwards_its_own_band() {
    let boxed: Box<dyn Provider> = Box::new(Banding);
    assert!(boxed.connection_info().capabilities.widens_mail_by_band());
    let cursor = SyncState::new("delta-7");
    let widening = widening();
    let chunks: Vec<_> = boxed
        .widen_email(&account(), &cursor, &widening, 50, 0)
        .collect()
        .await;
    let chunk = chunks.into_iter().next().unwrap().unwrap();
    assert_eq!(
        chunk.mode,
        PassMode::Additive,
        "the box fell back to the reopen default"
    );
    assert_eq!(chunk.advance_to.unwrap().as_str(), "delta-7");

    let recording: Box<dyn Provider> = Box::new(Recording {
        bands: true,
        ..Recording::default()
    });
    assert!(
        recording
            .connection_info()
            .capabilities
            .widens_mail_by_band()
    );
}

#[tokio::test]
async fn the_default_continues_its_own_fresh_pass_when_the_widening_resumes() {
    let provider = Recording::default();
    let widening = widening().resuming();
    let cursor = SyncState::new("fresh-page-3");
    let _: Vec<_> = provider
        .widen_email(&account(), &cursor, &widening, 50, 0)
        .collect()
        .await;
    assert_eq!(
        *provider.asked.lock().unwrap(),
        vec![(Some("fresh-page-3".to_owned()), widening.to().clone())],
        "a resumed reopen started over"
    );
}
