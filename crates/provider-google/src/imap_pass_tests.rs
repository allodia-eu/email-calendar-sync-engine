//! The IMAP pass over an in-memory Gmail: a first sync, mail arriving and changing, a move to
//! Trash, a deletion for good, and a folder renumbered.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::Message,
    membership::Memberships,
    sync::{SyncState, SyncWindow},
};
use engine_provider::{EmailChunk, PassMode, Provider, ProviderResult};
use futures_util::StreamExt;
use provider_imap::{GmailFolder, GmailFolderState, GmailMessage, GmailQuery, GmailState};
use serde_json::json;

use crate::{GmailProvider, imap_mail::ImapMail, test_support::fake_client};

/// One message as a folder holds it.
#[derive(Clone)]
struct Held {
    gmail_id: u64,
    labels: Vec<&'static str>,
    flags: Vec<&'static str>,
    modseq: u64,
}

struct Folder {
    validity: u32,
    uid_next: u32,
    messages: BTreeMap<u32, Held>,
}

/// A Gmail account's three folders, changed between passes by the test.
#[derive(Clone)]
struct FakeGmail(Arc<Mutex<BTreeMap<GmailFolder, Folder>>>, Arc<Mutex<u64>>);

impl FakeGmail {
    fn new() -> Self {
        let folders = GmailFolder::ALL
            .into_iter()
            .map(|f| {
                (
                    f,
                    Folder {
                        validity: 1,
                        uid_next: 1,
                        messages: BTreeMap::new(),
                    },
                )
            })
            .collect();
        Self(Arc::new(Mutex::new(folders)), Arc::new(Mutex::new(100)))
    }

    fn tick(&self) -> u64 {
        let mut modseq = self.1.lock().unwrap();
        *modseq += 1;
        *modseq
    }

    /// Delivers `held` into `folder`, returning its UID.
    fn deliver(&self, folder: GmailFolder, mut held: Held) -> u32 {
        held.modseq = self.tick();
        let mut folders = self.0.lock().unwrap();
        let f = folders.get_mut(&folder).unwrap();
        let uid = f.uid_next;
        f.uid_next += 1;
        f.messages.insert(uid, held);
        uid
    }

    fn take(&self, folder: GmailFolder, uid: u32) -> Held {
        self.0
            .lock()
            .unwrap()
            .get_mut(&folder)
            .unwrap()
            .messages
            .remove(&uid)
            .unwrap()
    }

    fn mark_read(&self, uid: u32) {
        let modseq = self.tick();
        let mut folders = self.0.lock().unwrap();
        let held = folders
            .get_mut(&GmailFolder::All)
            .unwrap()
            .messages
            .get_mut(&uid)
            .unwrap();
        held.flags.push("\\Seen");
        held.modseq = modseq;
    }

    fn with<T>(&self, folder: GmailFolder, read: impl FnOnce(&Folder) -> T) -> T {
        read(&self.0.lock().unwrap()[&folder])
    }
}

fn state_of(uid: u32, held: &Held) -> GmailState {
    GmailState {
        uid,
        gmail_id: held.gmail_id,
        thread_id: held.gmail_id,
        labels: held.labels.iter().map(|l| (*l).to_owned()).collect(),
        flags: held.flags.iter().map(|f| (*f).to_owned()).collect(),
        modseq: Some(held.modseq),
    }
}

#[async_trait]
impl ImapMail for FakeGmail {
    async fn open(&self, folder: GmailFolder) -> ProviderResult<Option<GmailFolderState>> {
        let modseq = *self.1.lock().unwrap();
        Ok(Some(self.with(folder, |f| GmailFolderState {
            uid_validity: f.validity,
            uid_next: f.uid_next,
            highest_modseq: modseq,
            exists: u32::try_from(f.messages.len()).unwrap(),
        })))
    }

    async fn search(
        &self,
        folder: GmailFolder,
        _validity: u32,
        query: &GmailQuery,
    ) -> ProviderResult<Vec<u32>> {
        // Gmail's own search (`X-GM-RAW`) is what found the inbox categories, which the pass no
        // longer reads.
        assert_eq!(query.raw, None, "the pass asks Gmail's search nothing");
        Ok(self.with(folder, |f| {
            f.messages
                .iter()
                .filter(|(uid, _)| query.uids.as_ref().is_none_or(|uids| uids.contains(uid)))
                .filter(|(uid, _)| query.from_uid.is_none_or(|from| **uid >= from))
                .map(|(uid, _)| *uid)
                .collect()
        }))
    }

    async fn messages(
        &self,
        folder: GmailFolder,
        _validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<GmailMessage>> {
        Ok(self.with(folder, |f| {
            uids.iter()
                .filter_map(|uid| f.messages.get(uid).map(|held| (uid, held)))
                .map(|(uid, held)| GmailMessage {
                    state: state_of(*uid, held),
                    message: Message::new(
                        MessageId::new(ProviderKey::new(format!("imap:v1:u{uid}@x")).unwrap()),
                        Memberships::of_one(MailboxId::try_from("x").unwrap()),
                    ),
                    date: None,
                })
                .collect()
        }))
    }

    async fn changed(
        &self,
        folder: GmailFolder,
        _validity: u32,
        below_uid: u32,
        modseq: u64,
    ) -> ProviderResult<Vec<GmailState>> {
        Ok(self.with(folder, |f| {
            f.messages
                .iter()
                .filter(|(uid, held)| **uid < below_uid && held.modseq > modseq)
                .map(|(uid, held)| state_of(*uid, held))
                .collect()
        }))
    }

    async fn ids(
        &self,
        folder: GmailFolder,
        _validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<(u32, u64)>> {
        Ok(self.with(folder, |f| {
            uids.iter()
                .filter_map(|uid| f.messages.get(uid).map(|held| (*uid, held.gmail_id)))
                .collect()
        }))
    }

    fn sources<'a>(&'a self, _gmail_ids: &'a [u64]) -> provider_imap::GmailSources<'a> {
        Box::pin(futures_util::stream::empty())
    }
}

fn held(gmail_id: u64, labels: Vec<&'static str>) -> Held {
    Held {
        gmail_id,
        labels,
        flags: Vec::new(),
        modseq: 0,
    }
}

fn provider(gmail: &FakeGmail) -> GmailProvider {
    let client = fake_client(vec![(
        "/labels",
        json!({ "labels": [{ "id": "Label_7", "name": "Work" }] }),
    )]);
    GmailProvider::new(client).with_imap_mail(Arc::new(gmail.clone()))
}

async fn pass(provider: &GmailProvider, cursor: Option<&SyncState>) -> Vec<EmailChunk> {
    let account = AccountId::try_from("a").unwrap();
    provider
        .stream_email(&account, cursor, SyncWindow::full(), 0, 0)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect()
}

fn last_cursor(chunks: &[EmailChunk]) -> SyncState {
    chunks.last().unwrap().advance_to.clone().unwrap()
}

fn places(message: &Message) -> Vec<&str> {
    message.mailboxes.iter().map(MailboxId::as_str).collect()
}

/// An account with one message in each folder, and the first pass's chunks.
async fn first_sync() -> (FakeGmail, GmailProvider, Vec<EmailChunk>) {
    let gmail = FakeGmail::new();
    gmail.deliver(GmailFolder::All, held(0xa1, vec!["\\Inbox", "Work"]));
    gmail.deliver(GmailFolder::Spam, held(0xb2, vec![]));
    gmail.deliver(GmailFolder::Trash, held(0xc3, vec!["\\Important"]));
    let provider = provider(&gmail);
    let chunks = pass(&provider, None).await;
    (gmail, provider, chunks)
}

#[tokio::test]
async fn a_first_sync_reads_all_three_folders_as_one_reconcile() {
    let (_, _, chunks) = first_sync().await;
    assert!(chunks.iter().all(|c| c.mode == PassMode::Reconcile));
    assert!(chunks.last().unwrap().is_reconcile_final());
    assert_eq!(chunks[0].total, Some(3));
    let messages: BTreeMap<&str, &Message> = chunks
        .iter()
        .flat_map(|c| &c.changed)
        .map(|m| (m.id.key().as_str(), m))
        .collect();
    assert_eq!(places(messages["a1"]), ["INBOX", "Label_7"]);
    assert_eq!(places(messages["b2"]), ["SPAM"]);
    assert_eq!(places(messages["c3"]), ["IMPORTANT", "TRASH"]);
    let present: Vec<&str> = chunks
        .iter()
        .flat_map(|c| &c.present)
        .map(ProviderKey::as_str)
        .collect();
    assert_eq!(present.len(), 3);
}

#[tokio::test]
async fn arrivals_and_changes_are_an_additive_delta() {
    let (gmail, provider, chunks) = first_sync().await;
    let cursor = last_cursor(&chunks);
    gmail.mark_read(1);
    gmail.deliver(GmailFolder::All, held(0xd4, vec!["\\Inbox"]));

    let delta = pass(&provider, Some(&cursor)).await;
    assert!(delta.iter().all(|c| c.mode == PassMode::Additive));
    let changed: Vec<&str> = delta
        .iter()
        .flat_map(|c| &c.changed)
        .map(|m| m.id.key().as_str())
        .collect();
    assert_eq!(changed, ["d4"]);
    let patched: Vec<&str> = delta
        .iter()
        .flat_map(|c| &c.patched)
        .map(|p| p.key.as_str())
        .collect();
    assert_eq!(patched, ["a1"]);
    assert_ne!(last_cursor(&delta), cursor);

    let quiet = pass(&provider, Some(&last_cursor(&delta))).await;
    assert!(
        quiet
            .iter()
            .all(|c| c.changed.is_empty() && c.patched.is_empty())
    );
}

#[tokio::test]
async fn a_move_to_trash_reconciles_and_keeps_the_message() {
    let (gmail, provider, chunks) = first_sync().await;
    let moved = gmail.take(GmailFolder::All, 1);
    gmail.deliver(GmailFolder::Trash, moved);

    let delta = pass(&provider, Some(&last_cursor(&chunks))).await;
    assert!(delta.iter().all(|c| c.mode == PassMode::Reconcile));
    let moved = delta
        .iter()
        .flat_map(|c| &c.changed)
        .find(|m| m.id.key().as_str() == "a1")
        .unwrap();
    assert!(places(moved).contains(&"TRASH"));
    let present: Vec<&str> = delta
        .iter()
        .flat_map(|c| &c.present)
        .map(ProviderKey::as_str)
        .collect();
    assert!(present.contains(&"a1"), "{present:?}");
}

#[tokio::test]
async fn a_deletion_for_good_leaves_the_message_out_of_the_reconcile() {
    let (gmail, provider, chunks) = first_sync().await;
    gmail.take(GmailFolder::Trash, 1);

    let delta = pass(&provider, Some(&last_cursor(&chunks))).await;
    assert!(delta.last().unwrap().is_reconcile_final());
    let present: Vec<&str> = delta
        .iter()
        .flat_map(|c| &c.present)
        .map(ProviderKey::as_str)
        .collect();
    assert_eq!(present.len(), 2);
    assert!(!present.contains(&"c3"), "{present:?}");
}

#[tokio::test]
async fn a_renumbered_folder_or_an_api_cursor_starts_afresh() {
    let (gmail, provider, chunks) = first_sync().await;
    gmail
        .0
        .lock()
        .unwrap()
        .get_mut(&GmailFolder::Spam)
        .unwrap()
        .validity = 2;
    let after_reset = pass(&provider, Some(&last_cursor(&chunks))).await;
    assert_eq!(after_reset[0].total, Some(3));

    let after_api = pass(&provider, Some(&SyncState::new("234042"))).await;
    assert_eq!(after_api[0].total, Some(3));
    assert!(after_api.last().unwrap().is_reconcile_final());
}

#[tokio::test]
async fn the_imap_side_reads_bodies_in_batches_over_several_connections() {
    let gmail = FakeGmail::new();
    let info = provider(&gmail).connection_info();
    assert_eq!((info.concurrent_fetches, info.sources_per_request), (4, 50));
    let api = GmailProvider::new(fake_client(Vec::new())).connection_info();
    assert_eq!((api.concurrent_fetches, api.sources_per_request), (20, 1));
}
