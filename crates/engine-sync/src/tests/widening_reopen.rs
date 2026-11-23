//! An adapter on the default `widen_email` (the reopen) whose fresh pass is a resumable backfill,
//! as Gmail's is: a reopen killed mid-way must be continued by the next pass, not started over on
//! every one, or the widening never ends and costs the window again each time.

use engine_core::{
    sync::{MailboxWindows, StampedCursor},
    time::CalendarDate,
};
use engine_provider::{CalendarSync, CalendarWrites};

use super::*;

fn dated(id: &str, received: &str) -> Message {
    let mut m = message(id, "INBOX", id);
    m.received_at = Some(received.parse().unwrap());
    m
}

struct Reopening {
    messages: Vec<Message>,
    fresh_starts: Mutex<usize>,
    fail_after: Mutex<Option<usize>>,
}

#[async_trait::async_trait]
impl Provider for Reopening {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Email,
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let mb = vec![mailbox("INBOX", "Inbox", Some(MailboxRole::Inbox))];
        let present = mb.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mb, present),
            SyncState::new("mailboxes"),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: MailboxWindows,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        // Cursor: "done" (delta), or "page=<n>" (a backfill in progress).
        let start = match cursor.map(SyncState::as_str) {
            Some("done") => {
                let chunk =
                    EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("done"));
                return Box::pin(futures_util::stream::iter(vec![Ok(chunk)]));
            }
            Some(page) => page.strip_prefix("page=").unwrap().parse().unwrap(),
            None => {
                *self.fresh_starts.lock().unwrap() += 1;
                0
            }
        };
        let listing: Vec<Message> = self
            .messages
            .iter()
            .filter(|m| window.admits(m.mailboxes.iter(), m.received_at))
            .cloned()
            .collect();
        let fail_after = if start == 0 {
            self.fail_after.lock().unwrap().take()
        } else {
            None
        };
        let mut items = Vec::new();
        for (at, m) in listing.iter().enumerate().skip(start) {
            if fail_after.is_some_and(|after| at - start == after) {
                items.push(Err(ProviderError::retryable("killed mid-backfill")));
                return Box::pin(futures_util::stream::iter(items));
            }
            let next = if at + 1 == listing.len() {
                "done".to_owned()
            } else {
                format!("page={}", at + 1)
            };
            items.push(Ok(EmailChunk::additive(
                vec![m.clone()],
                Vec::new(),
                Some(listing.len()),
                SyncState::new(next),
            )));
        }
        if items.is_empty() {
            items.push(Ok(EmailChunk::additive(
                Vec::new(),
                Vec::new(),
                Some(0),
                SyncState::new("done"),
            )));
        }
        Box::pin(futures_util::stream::iter(items))
    }
}
impl CalendarSync for Reopening {}
impl CalendarWrites for Reopening {}
impl engine_provider::MailboxWrites for Reopening {}

fn since(month: u8) -> MailboxWindows {
    SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap()).into()
}

async fn pass(p: &Reopening, store: &SqliteStore<ManualClock>, window: MailboxWindows) -> bool {
    sync_mail(
        core::slice::from_ref(p),
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 1).within(window),
        &IgnoreCommits,
    )
    .await
    .is_ok()
}

#[tokio::test]
async fn a_reopen_killed_mid_way_is_continued_and_ends() {
    let p = Reopening {
        messages: vec![
            dated("m1", "2026-06-10T09:00:00Z"),
            dated("m2", "2026-03-10T09:00:00Z"),
            dated("m3", "2026-02-10T09:00:00Z"),
            dated("m4", "2026-01-10T09:00:00Z"),
        ],
        fresh_starts: Mutex::new(0),
        fail_after: Mutex::new(None),
    };
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let scope = SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::Email,
    };
    assert!(pass(&p, &store, since(4)).await);
    assert_eq!(*p.fresh_starts.lock().unwrap(), 1);
    // The first widening pass is killed two chunks into its reopen; the next continues it.
    *p.fail_after.lock().unwrap() = Some(2);
    assert!(!pass(&p, &store, since(1)).await);
    assert!(pass(&p, &store, since(1)).await);
    assert_eq!(
        *p.fresh_starts.lock().unwrap(),
        2,
        "the reopen started over instead of continuing"
    );
    let stamp = StampedCursor::parse(
        store
            .load_sync_state(account(), &scope)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(stamp.covered(), &since(1), "never reached the wider depth");
}
