//! An account-wide mail adapter that widens by band, the shape `provider-jmap` takes: a fresh
//! pass reconciles the window, a delta reports what changed since its version, and
//! `widen_email` lists only what the widening's bands admit, checkpointing its position in them.

use engine_core::sync::{MailboxWindows, Widening};
use engine_provider::{CalendarSync, CalendarWrites};

use super::*;

/// The server's mail: each message with the version that last changed it.
#[derive(Default)]
pub(super) struct Server {
    pub(super) messages: Vec<(Message, u32)>,
    pub(super) version: u32,
}

impl Server {
    /// Stores `message` as a change at the next version.
    pub(super) fn put(&mut self, message: Message) {
        self.version += 1;
        let version = self.version;
        self.messages.retain(|(held, _)| held.id != message.id);
        self.messages.push((message, version));
    }
}

/// A band position as this adapter writes it: `s<version>|band=<bands>|at=<listed>`.
fn position(version: u32, band: &str, at: usize) -> SyncState {
    SyncState::new(format!("s{version}|band={band}|at={at}"))
}

/// The version a cursor was written at, and its band position if it has one.
fn read(cursor: &SyncState) -> (u32, Option<(String, usize)>) {
    let raw = cursor
        .as_str()
        .strip_prefix('s')
        .expect("this adapter's own cursor");
    let Some((version, rest)) = raw.split_once("|band=") else {
        return (raw.parse().unwrap(), None);
    };
    let (band, at) = rest.split_once("|at=").unwrap();
    (
        version.parse().unwrap(),
        Some((band.to_owned(), at.parse().unwrap())),
    )
}

pub(super) struct Banded {
    pub(super) server: Mutex<Server>,
    pub(super) mailboxes: Vec<Mailbox>,
    /// Each `widen_email` call's starting position in its listing.
    pub(super) widen_starts: Mutex<Vec<usize>>,
    /// Every key a band chunk carried.
    pub(super) band_fetched: Mutex<Vec<String>>,
    /// Fail the next band after this many chunks.
    pub(super) fail_after: Mutex<Option<usize>>,
    /// Wait this long before each band chunk.
    pub(super) pace: Option<Duration>,
}

impl Banded {
    pub(super) fn new(messages: Vec<Message>) -> Self {
        let mut server = Server::default();
        for message in messages {
            server.put(message);
        }
        Self {
            server: Mutex::new(server),
            mailboxes: vec![
                mailbox("INBOX", "Inbox", Some(MailboxRole::Inbox)),
                mailbox("Sent", "Sent", Some(MailboxRole::Sent)),
            ],
            widen_starts: Mutex::new(Vec::new()),
            band_fetched: Mutex::new(Vec::new()),
            fail_after: Mutex::new(None),
            pace: None,
        }
    }
}

fn band_key(widening: &Widening) -> String {
    widening
        .bands()
        .iter()
        .map(|band| {
            format!(
                "{:?}..{:?}@{:?}",
                band.since(),
                band.before(),
                band.mailbox().map(MailboxId::as_str)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
        .replace('|', "/")
}

#[async_trait::async_trait]
impl Provider for Banded {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_band_widening())
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
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
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
        let server = self.server.lock().unwrap();
        let now = server.version;
        let chunk = match cursor.map(read) {
            None => {
                let fetched: Vec<Message> = server
                    .messages
                    .iter()
                    .map(|(message, _)| message)
                    .filter(|m| window.admits(m.mailboxes.iter(), m.received_at))
                    .cloned()
                    .collect();
                let present = fetched.iter().map(|m| m.id.key().clone()).collect();
                EmailChunk::reconcile_last(
                    fetched,
                    present,
                    None,
                    SyncState::new(format!("s{now}")),
                )
            }
            Some((since, band)) => {
                let changed = server
                    .messages
                    .iter()
                    .filter(|(_, version)| *version > since)
                    .map(|(message, _)| message.clone())
                    .collect();
                // An additive delta carries a band position through, as the adapters do.
                let next = match band {
                    Some((band, at)) => position(now, &band, at),
                    None => SyncState::new(format!("s{now}")),
                };
                EmailChunk::additive(changed, Vec::new(), None, next)
            }
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }

    fn widen_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: &'a SyncState,
        widening: &'a Widening,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let (version, recorded) = read(cursor);
        let key = band_key(widening);
        // The caller says whether to continue; the band key is only what the position names.
        let start = recorded
            .filter(|_| widening.resumes())
            .map_or(0, |(_, at)| at);
        self.widen_starts.lock().unwrap().push(start);
        let bands = widening.bands();
        let mut listing: Vec<Message> = self
            .server
            .lock()
            .unwrap()
            .messages
            .iter()
            .map(|(message, _)| message)
            .filter(|m| {
                bands
                    .iter()
                    .any(|b| b.admits(m.mailboxes.iter(), m.received_at))
            })
            .cloned()
            .collect();
        listing.sort_by_key(|m| core::cmp::Reverse(m.received_at));
        let total = listing.len();
        let fail_after = self.fail_after.lock().unwrap().take();
        let mut items = Vec::new();
        for (at, message) in listing.into_iter().enumerate().skip(start) {
            if fail_after.is_some_and(|after| at - start == after) {
                items.push(Err(ProviderError::retryable("killed mid-band")));
                break;
            }
            self.band_fetched
                .lock()
                .unwrap()
                .push(message.id.key().as_str().to_owned());
            let next = if at + 1 == total {
                SyncState::new(format!("s{version}"))
            } else {
                position(version, &key, at + 1)
            };
            items.push(Ok(EmailChunk::additive(
                vec![message],
                Vec::new(),
                Some(total),
                next,
            )));
        }
        if start >= total {
            let done = SyncState::new(format!("s{version}"));
            items.push(Ok(EmailChunk::additive(
                Vec::new(),
                Vec::new(),
                Some(total),
                done,
            )));
        }
        let pace = self.pace;
        Box::pin(async_stream::stream! {
            for item in items {
                if let Some(pace) = pace {
                    tokio::time::sleep(pace).await;
                }
                yield item;
            }
        })
    }
}

impl CalendarSync for Banded {}
impl CalendarWrites for Banded {}
impl engine_provider::MailboxWrites for Banded {}
