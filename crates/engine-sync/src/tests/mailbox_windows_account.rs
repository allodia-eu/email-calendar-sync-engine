//! A mailbox held further back than its account, where one scope holds the whole account's mail
//! (JMAP, Gmail): the adapter is handed every mailbox's window, and deepening one starts that
//! scope over with no cursor cleared by the host.

use engine_core::{sync::MailboxWindows, time::CalendarDate};
use engine_provider::CalendarWrites;

use super::*;

/// Every pass the scope ran: whether it started over (no cursor), and the windows it was handed.
type Passes = Arc<Mutex<Vec<(bool, MailboxWindows)>>>;

/// One account-wide email scope. A pass with no cursor is a snapshot holding what the window of
/// any mailbox a message is filed in admits, as a server's own filter would; a resumed one is an
/// empty delta, so whatever a test finds stored, a snapshot fetched.
struct Account {
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    passes: Passes,
}

#[async_trait::async_trait]
impl Provider for Account {
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
        self.passes
            .lock()
            .unwrap()
            .push((cursor.is_none(), window.clone()));
        let chunk = if cursor.is_some() {
            EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("state"))
        } else {
            let fetched: Vec<Message> = self
                .messages
                .iter()
                .filter(|m| window.admits(m.mailboxes.iter(), m.received_at))
                .cloned()
                .collect();
            let present = fetched.iter().map(|m| m.id.key().clone()).collect();
            EmailChunk::reconcile_last(fetched, present, None, SyncState::new("state"))
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

impl CalendarSync for Account {}

impl CalendarWrites for Account {}
impl engine_provider::MailboxWrites for Account {}

fn dated(id: &str, mailbox: &str, received: &str) -> Message {
    let mut message = message(id, mailbox, id);
    message.received_at = Some(received.parse().unwrap());
    message
}

fn since(year: i32, month: u8, day: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, day).unwrap())
}

fn account_window() -> MailboxWindows {
    MailboxWindows::new(since(2025, 6, 1))
}

/// The account keeps a year; Sent is held back three.
fn deepened() -> MailboxWindows {
    account_window().deepen(MailboxId::try_from("Sent").unwrap(), since(2023, 6, 1))
}

fn provider() -> (Account, Passes) {
    let passes: Passes = Arc::new(Mutex::new(Vec::new()));
    let provider = Account {
        mailboxes: vec![
            mailbox("INBOX", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("Sent", "Sent", Some(MailboxRole::Sent)),
        ],
        messages: vec![
            dated("in-new", "INBOX", "2026-02-01T09:00:00Z"),
            dated("in-old", "INBOX", "2024-02-01T09:00:00Z"),
            dated("sent-new", "Sent", "2026-02-01T09:00:00Z"),
            dated("sent-old", "Sent", "2024-02-01T09:00:00Z"),
            dated("sent-ancient", "Sent", "2022-02-01T09:00:00Z"),
        ],
        passes: Arc::clone(&passes),
    };
    (provider, passes)
}

async fn pass(provider: &Account, store: &SqliteStore<ManualClock>, window: MailboxWindows) {
    let report = sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0).within(window),
        &IgnoreCommits,
    )
    .await;
    assert!(report.is_ok(), "{report:?}");
}

async fn stored(store: &SqliteStore<ManualClock>, provider: &Account) -> Vec<String> {
    store
        .object_keys(&provider.email_scope(&account()))
        .await
        .unwrap()
        .iter()
        .map(|key| key.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn deepening_a_mailbox_starts_an_account_wide_scope_over_and_fetches_only_that_mailbox() {
    let (provider, passes) = provider();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    pass(&provider, &store, account_window()).await;
    assert_eq!(stored(&store, &provider).await, vec!["in-new", "sent-new"]);

    pass(&provider, &store, deepened()).await;
    assert_eq!(
        stored(&store, &provider).await,
        vec!["in-new", "sent-new", "sent-old"],
        "Sent reaches back to its own floor, the Inbox does not, and nothing past Sent's floor"
    );
    assert_eq!(
        passes.lock().unwrap()[1],
        (true, deepened()),
        "the scope started over, handed every mailbox's window"
    );
}

#[tokio::test]
async fn an_account_wide_scope_resumes_once_it_holds_the_deepened_mailbox() {
    let (provider, passes) = provider();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    for window in [
        deepened(),
        deepened(),
        // Sent held less far back, then no longer at all: nothing more is wanted.
        account_window().deepen(MailboxId::try_from("Sent").unwrap(), since(2024, 6, 1)),
        account_window(),
    ] {
        pass(&provider, &store, window).await;
    }

    let fresh: Vec<bool> = passes.lock().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(fresh, [true, false, false, false]);
    assert_eq!(
        stored(&store, &provider).await,
        vec!["in-new", "sent-new", "sent-old"],
        "a narrower window removes nothing; that is the prune's job"
    );
}

#[tokio::test]
async fn deepening_a_second_mailbox_starts_the_scope_over_again() {
    let (provider, passes) = provider();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    pass(&provider, &store, deepened()).await;
    pass(
        &provider,
        &store,
        deepened().deepen(MailboxId::try_from("INBOX").unwrap(), since(2024, 1, 1)),
    )
    .await;

    let fresh: Vec<bool> = passes.lock().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(fresh, [true, true]);
    assert_eq!(
        stored(&store, &provider).await,
        vec!["in-new", "in-old", "sent-new", "sent-old"]
    );
}
