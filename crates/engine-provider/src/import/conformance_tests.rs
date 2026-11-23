//! The conformance check against an in-memory target: it passes a target that keeps its word,
//! and fails one that does not, so a live suite that passes it has been checked.

use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::{Keyword, Message},
    membership::Memberships,
    raw::RawMime,
    sync::{SyncState, SyncUpdate},
    time::UtcDateTime,
};
use futures_util::FutureExt;

use super::{
    super::{ImportedMessage, MailImportLimits, MailImporter, MessageImport},
    raw_message, stores_as_sent,
};
use crate::{
    CalendarSync, CalendarWrites, Capabilities, ConnectionInfo, MailboxWrites, Provider,
    ProviderResult, ScopeSync,
};

/// How the fake target misbehaves, if at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    /// Stores every keyword but `$label1`, and says it kept them all.
    DropsAKeywordSilently,
    /// Stores the bytes with a header rewritten.
    RewritesTheBytes,
    /// Files every message in a mailbox other than the one asked for.
    MisFiles,
    /// Stamps a message sent without an arrival time with a time long past.
    StampsAStaleTime,
    /// Answers every message as already there, though it stored it now.
    CallsEverythingAlreadyThere,
}

/// A target holding what it stored, in order; its cursor is how many it held.
struct Target {
    stored: Arc<Mutex<Vec<(Message, RawMime)>>>,
    fault: Fault,
}

impl Target {
    fn new(fault: Fault) -> Self {
        Self {
            stored: Arc::default(),
            fault,
        }
    }
}

#[async_trait]
impl MailImporter for Target {
    async fn mail_import_limits(&self, _: &AccountId) -> ProviderResult<MailImportLimits> {
        Ok(MailImportLimits::new(std::num::NonZeroUsize::MIN))
    }

    async fn import_cursor(&self, _: &AccountId, _: &MailboxId) -> ProviderResult<SyncState> {
        let held = self.stored.lock().unwrap().len();
        Ok(SyncState::new(held.to_string()))
    }

    async fn import_messages(
        &self,
        _: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        let mut stored = self.stored.lock().unwrap();
        batch
            .iter()
            .map(|import| {
                let id = format!("m{}", stored.len());
                let mailboxes = if self.fault == Fault::MisFiles {
                    Memberships::of_one(MailboxId::try_from("Elsewhere").unwrap())
                } else {
                    import.mailboxes.clone()
                };
                let mut message =
                    Message::new(MessageId::try_from(id.as_str()).unwrap(), mailboxes);
                message.keywords.clone_from(&import.metadata.keywords);
                if self.fault == Fault::DropsAKeywordSilently {
                    message.keywords.remove(&Keyword::new("$label1").unwrap());
                }
                let stamped = if self.fault == Fault::StampsAStaleTime {
                    "2001-01-01T00:00:00Z".parse::<UtcDateTime>().unwrap()
                } else {
                    super::utc_now()
                };
                message.received_at = Some(import.metadata.received_at.unwrap_or(stamped));
                let raw = if self.fault == Fault::RewritesTheBytes {
                    RawMime::new(import.raw.as_bytes().to_ascii_uppercase())
                } else {
                    import.raw.clone()
                };
                stored.push((message, raw));
                let key = ProviderKey::new(id).unwrap();
                if self.fault == Fault::CallsEverythingAlreadyThere {
                    return Ok(ImportedMessage::existing(key));
                }
                Ok(ImportedMessage::created(Some(key)).with_kept(import.metadata.clone()))
            })
            .collect()
    }
}

#[async_trait]
impl Provider for Target {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    async fn sync_email(
        &self,
        _: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Message>> {
        let stored = self.stored.lock().unwrap();
        let from: usize = cursor.map_or(0, |cursor| cursor.as_str().parse().unwrap());
        let changed = stored[from..].iter().map(|(m, _)| m.clone()).collect();
        let next = SyncState::new(stored.len().to_string());
        Ok(ScopeSync::new(
            SyncUpdate::Delta {
                changed,
                patched: Vec::new(),
                removed: Vec::new(),
            },
            next,
        ))
    }

    async fn fetch_message_source(
        &self,
        _: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        let stored = self.stored.lock().unwrap();
        let (_, raw) = stored.iter().find(|(m, _)| m.id == message.id).unwrap();
        Ok(raw.clone())
    }
}

impl CalendarSync for Target {}
impl CalendarWrites for Target {}
impl MailboxWrites for Target {}

/// Runs the kit against a target with `fault` and answers the claim it failed, or `None` where
/// it passed.
async fn failed_claim(fault: Fault) -> Option<String> {
    let target = Target::new(fault);
    let account = AccountId::try_from("acct").unwrap();
    let folder = MailboxId::try_from("Archive").unwrap();
    let run = AssertUnwindSafe(stores_as_sent(
        "fake", &target, &target, &account, &folder, 7,
    ))
    .catch_unwind()
    .await;
    let payload = run.err()?;
    Some(
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default(),
    )
}

#[tokio::test]
async fn a_target_that_keeps_its_word_passes() {
    assert_eq!(failed_claim(Fault::None).await, None);
}

/// Each lie is caught by the claim it breaks, not by some other assertion along the way.
#[tokio::test]
async fn each_broken_claim_is_named() {
    for (fault, claim) in [
        (
            Fault::DropsAKeywordSilently,
            "the receipt says exactly what was stored",
        ),
        (Fault::RewritesTheBytes, "stored byte for byte"),
        (Fault::MisFiles, "filed in the mailbox asked for"),
        (Fault::StampsAStaleTime, "the server's own time"),
        (
            Fault::CallsEverythingAlreadyThere,
            "stored now, not already there",
        ),
    ] {
        let failed = failed_claim(fault).await.expect("the kit fails it");
        assert!(failed.contains(claim), "{fault:?} failed on: {failed}");
    }
}

#[tokio::test]
async fn the_receipts_say_what_was_kept() {
    let target = Target::new(Fault::None);
    let account = AccountId::try_from("acct").unwrap();
    let folder = MailboxId::try_from("Archive").unwrap();
    let receipts = stores_as_sent("fake", &target, &target, &account, &folder, 7).await;
    assert_eq!(receipts.dated.kept.keywords, receipts.asked);
    assert_eq!(receipts.undated.kept.keywords.len(), 0);
}

#[test]
fn each_run_writes_its_own_message() {
    let one = raw_message("dated", 1);
    assert_ne!(one, raw_message("dated", 2));
    assert!(String::from_utf8_lossy(one.as_bytes()).contains("example.com"));
}
