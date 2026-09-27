//! A mailbox held further back than its account: what the pass fetches for it, what it keeps,
//! and that no other mailbox moves.

use engine_core::{sync::MailboxWindows, time::CalendarDate};
use engine_provider::CalendarWrites;

use super::*;

/// Every window a folder was asked to fetch under, by folder.
type Asked = Arc<Mutex<Vec<(MailboxId, SyncWindow)>>>;

/// One IMAP-shaped folder whose first pass is a fresh backfill, the shape `provider-imap` streams:
/// its older group commits as an **additive** checkpoint, and its newest completes with a
/// reconcile over everything the pass enumerated. It serves only what the window it was asked for
/// admits, as a server's own date filter would, and records that window.
struct Folder {
    name: MailboxId,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    asked: Asked,
}

impl Folder {
    fn new(name: &str, mailboxes: &[Mailbox], messages: Vec<Message>, asked: &Asked) -> Self {
        Self {
            name: MailboxId::try_from(name).unwrap(),
            mailboxes: mailboxes.to_vec(),
            messages,
            asked: Arc::clone(asked),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Folder {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailboxList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: self.name.clone(),
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
            SyncState::new("folders"),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        self.asked.lock().unwrap().push((self.name.clone(), window));
        if cursor.is_some() {
            let done = EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("done"));
            return Box::pin(futures_util::stream::iter(vec![Ok(done)]));
        }
        let mut fetched: Vec<Message> = self
            .messages
            .iter()
            .filter(|m| window.admits(m.received_at))
            .cloned()
            .collect();
        let present: Vec<ProviderKey> = fetched.iter().map(|m| m.id.key().clone()).collect();
        let older = fetched.split_off(fetched.len().min(1));
        let chunks = vec![
            // The older group: committed and checkpointed before the pass completes.
            Ok(EmailChunk::additive(
                older,
                Vec::new(),
                None,
                SyncState::new("low"),
            )),
            Ok(EmailChunk::reconcile_last(
                fetched,
                present,
                None,
                SyncState::new("done"),
            )),
        ];
        Box::pin(futures_util::stream::iter(chunks))
    }
}

impl CalendarWrites for Folder {}

fn dated(id: &str, mailbox: &str, received: &str) -> Message {
    let mut message = message(id, mailbox, id);
    message.received_at = Some(received.parse().unwrap());
    message
}

fn since(year: i32, month: u8, day: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, day).unwrap())
}

fn sent_id() -> MailboxId {
    MailboxId::try_from("Sent").unwrap()
}

/// The account keeps a year; Sent is held back three.
fn deepened() -> MailboxWindows {
    MailboxWindows::new(since(2025, 6, 1)).deepen(sent_id(), since(2023, 6, 1))
}

/// An Inbox and a Sent folder, each holding one recent and one old message, and the log of the
/// windows they were asked for.
fn two_folders() -> (Vec<Folder>, Asked) {
    let asked: Asked = Arc::new(Mutex::new(Vec::new()));
    let boxes = [
        mailbox("INBOX", "Inbox", Some(MailboxRole::Inbox)),
        mailbox("Sent", "Sent", Some(MailboxRole::Sent)),
    ];
    let folders = vec![
        Folder::new(
            "INBOX",
            &boxes,
            vec![
                dated("in-new", "INBOX", "2026-02-01T09:00:00Z"),
                dated("in-old", "INBOX", "2024-02-01T09:00:00Z"),
            ],
            &asked,
        ),
        Folder::new(
            "Sent",
            &boxes,
            vec![
                dated("sent-new", "Sent", "2026-02-01T09:00:00Z"),
                dated("sent-old", "Sent", "2024-02-01T09:00:00Z"),
            ],
            &asked,
        ),
    ];
    (folders, asked)
}

async fn stored(store: &SqliteStore<ManualClock>, folder: &Folder) -> Vec<String> {
    store
        .object_keys(&folder.email_scope(&account()))
        .await
        .unwrap()
        .iter()
        .map(|key| key.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn a_deepened_folder_is_fetched_further_back_and_no_other_folder_is() {
    let (folders, asked) = two_folders();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync_mail(
        &folders,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0).within(deepened()),
        &IgnoreCommits,
    )
    .await;
    assert!(report.is_ok(), "{report:?}");

    let mut asked = asked.lock().unwrap().clone();
    asked.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        asked,
        vec![
            (MailboxId::try_from("INBOX").unwrap(), since(2025, 6, 1)),
            (sent_id(), since(2023, 6, 1)),
        ],
        "each folder fetches under its own window"
    );
    assert_eq!(stored(&store, &folders[0]).await, vec!["in-new"]);
    // The old Sent message arrives in the backfill's additive group. Admitting it by the
    // account's window would drop it there while its key stayed in the reconcile's present set:
    // fetched, never stored, and never tombstoned either.
    assert_eq!(
        stored(&store, &folders[1]).await,
        vec!["sent-new", "sent-old"]
    );
}

#[tokio::test]
async fn without_the_override_the_same_pass_keeps_the_account_window() {
    // The control for the test above: same folders, same pass, no deepened mailbox.
    let (folders, asked) = two_folders();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    sync_mail(
        &folders,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0).within(since(2025, 6, 1)),
        &IgnoreCommits,
    )
    .await;

    assert!(
        asked
            .lock()
            .unwrap()
            .iter()
            .all(|(_, window)| *window == since(2025, 6, 1))
    );
    assert_eq!(stored(&store, &folders[1]).await, vec!["sent-new"]);
}

#[tokio::test]
async fn coverage_records_the_account_window_not_a_deepened_mailboxs() {
    // Coverage says how far back every mailbox's recipients were observed, so a deeper Sent
    // must not claim that depth for the account.
    let (folders, _asked) = two_folders();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    sync_mail(
        &folders,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0).within(deepened()),
        &IgnoreCommits,
    )
    .await;

    let coverage = store.recipient_coverage(Some(account())).await.unwrap();
    assert_eq!(coverage.len(), 1);
    assert_eq!(coverage[0].window, since(2025, 6, 1));
}

#[tokio::test]
async fn an_account_wide_delta_keeps_an_old_arrival_only_in_the_deepened_mailbox() {
    // JMAP- and Gmail-shaped: one scope for the whole account, so the fetch cannot be told which
    // mailbox is deeper, but each arrival still carries the mailboxes it is filed in.
    let provider = FakeMail::new(
        vec![
            mailbox("INBOX", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("Sent", "Sent", Some(MailboxRole::Sent)),
        ],
        vec![dated("recent", "INBOX", "2026-02-01T09:00:00Z")],
    );
    *provider.delta.lock().unwrap() = vec![
        dated("filed-in-sent", "Sent", "2024-02-01T09:00:00Z"),
        dated("filed-in-inbox", "INBOX", "2024-02-01T09:00:00Z"),
        dated("filed-too-old", "Sent", "2022-02-01T09:00:00Z"),
    ];
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    for _ in 0..2 {
        sync_mail(
            core::slice::from_ref(&provider),
            &store,
            &account(),
            worker(),
            Duration::from_mins(1),
            StreamTuning::new(0, 0).within(deepened()),
            &IgnoreCommits,
        )
        .await;
    }

    let keys: Vec<String> = store
        .object_keys(&provider.email_scope(&account()))
        .await
        .unwrap()
        .iter()
        .map(|key| key.as_str().to_owned())
        .collect();
    assert_eq!(keys, vec!["filed-in-sent", "recent"]);
}
