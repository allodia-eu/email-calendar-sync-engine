//! What every [`MailImporter`] must do against a real target, checked the same way for each
//! adapter (the `import-conformance` feature, for live test suites only).
//!
//! One question, one answer: the claims an import makes are the trait's, not an adapter's, so
//! they are written once here and each adapter's live suite runs them against its servers. An
//! adapter adds only what is its own (IMAP's one mailbox per message, JMAP's several).
//!
//! Every claim is read back through the sync seam, never taken from the import's own answer:
//!
//! - the bytes are stored byte for byte, in the mailbox asked for, as a message stored now rather
//!   than one already there;
//! - the arrival time is the one sent, and an undated control carries the server's own time (within
//!   a day of the clock running the check), so a pass cannot come from a server that happened to
//!   stamp the right day;
//! - of the keywords asked for, exactly those the receipt says were kept are stored (a server may
//!   add its own beside them: Stalwart files every message `$hasattachment` or `$hasnoattachment`);
//! - a delta from the [import cursor](MailImporter::import_cursor) lists exactly the imports.
//!
//! The checks panic on failure, naming the target's `label`: they are assertions for a test.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Keyword, Message, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    sync::SyncUpdate,
    time::UtcDateTime,
};

use super::{ImportedMessage, MailImporter, MessageImport};
use crate::Provider;

/// The receipts of the dated message and of the undated control, for an adapter to assert what
/// is its own beside the shared claims (such as every keyword kept, where its harness keeps
/// them all).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Receipts {
    /// The answer for the message sent with keywords and an arrival time.
    pub dated: ImportedMessage,
    /// The answer for the message sent with neither.
    pub undated: ImportedMessage,
    /// The keywords the dated message asked for.
    pub asked: BTreeSet<Keyword>,
}

/// A small message whose bytes are unique to `run`, so a read-back can only match it.
#[must_use]
pub fn raw_message(subject: &str, run: u128) -> RawMime {
    RawMime::new(format!(
        "From: Sender <sender@example.com>\r\n\
         To: Receiver <receiver@example.com>\r\n\
         Subject: {subject}\r\n\
         Date: Sun, 3 Feb 2019 05:05:06 +0100\r\n\
         Message-ID: <import-{subject}-{run}@example.com>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         Imported as it was, run {run}.\r\n"
    ))
}

/// Imports a dated message carrying keywords and an undated control into `folder`, then reads
/// both back through `reader` as a delta from the import cursor, and checks every shared claim.
///
/// `reader` is a provider whose `stream_email` covers `folder`: the importer itself where its
/// cursor covers the account (JMAP), one bound to `folder` where it is per folder (IMAP). The
/// target must name the key of what it stores.
///
/// # Panics
///
/// When the target breaks a claim, naming `label`.
pub async fn stores_as_sent(
    label: &str,
    importer: &dyn MailImporter,
    reader: &dyn Provider,
    account: &AccountId,
    folder: &MailboxId,
    run: u128,
) -> Receipts {
    let cursor = importer
        .import_cursor(account, folder)
        .await
        .unwrap_or_else(|err| panic!("import cursor on {label}: {err}"));

    let received = "2019-02-03T04:05:06Z"
        .parse::<UtcDateTime>()
        .expect("a date");
    let asked: BTreeSet<Keyword> = ["$seen", "$flagged", "$answered", "$label1"]
        .into_iter()
        .map(|name| Keyword::new(name).expect("a keyword"))
        .collect();
    let dated = MessageImport::new(
        raw_message("dated", run),
        Memberships::of_one(folder.clone()),
    )
    .with_metadata(
        MessageMetadata::default()
            .with_keywords(asked.clone())
            .with_received_at(received),
    );
    let undated = MessageImport::new(
        raw_message("undated", run),
        Memberships::of_one(folder.clone()),
    );
    let batch = [dated, undated];

    let answers = importer.import_messages(account, &batch).await;
    assert_eq!(answers.len(), 2, "{label}: one answer per message");
    let mut answers = answers
        .into_iter()
        .map(|answer| answer.unwrap_or_else(|err| panic!("import on {label}: {err}")));
    let (dated_answer, undated_answer) =
        (answers.next().expect("two"), answers.next().expect("two"));
    for answer in [&dated_answer, &undated_answer] {
        assert!(
            !answer.already_there,
            "{label}: stored now, not already there"
        );
    }
    assert!(
        dated_answer.kept.keywords.is_subset(&asked),
        "{label}: a receipt keeps only what was asked"
    );
    assert_eq!(dated_answer.kept.received_at, Some(received), "{label}");
    assert_eq!(undated_answer.kept.received_at, None, "{label}");

    let scope = reader
        .sync_email(account, Some(&cursor))
        .await
        .unwrap_or_else(|err| panic!("delta from the import cursor on {label}: {err}"));
    let SyncUpdate::Delta { changed, .. } = scope.update else {
        panic!("{label}: a pass from the import cursor is a delta");
    };
    assert_eq!(
        changed.len(),
        2,
        "{label}: the delta lists exactly the two imports"
    );
    let find = |key: Option<&ProviderKey>| -> Message {
        let key = key.unwrap_or_else(|| panic!("{label}: the target names the key"));
        changed
            .iter()
            .find(|message| message.id.key() == key)
            .cloned()
            .unwrap_or_else(|| panic!("{label}: the delta lists {key}"))
    };
    let dated_back = find(dated_answer.key.as_ref());
    let undated_back = find(undated_answer.key.as_ref());
    for back in [&dated_back, &undated_back] {
        assert!(
            back.mailboxes.iter().any(|mailbox| mailbox == folder),
            "{label}: filed in the mailbox asked for, got {:?}",
            back.mailboxes
        );
    }

    assert_eq!(
        dated_back.received_at,
        Some(received),
        "{label}: the arrival time as sent"
    );
    let control = undated_back.received_at.expect("the server stamps one");
    let now = utc_now();
    let (earliest, latest) = (shifted(now, -1), shifted(now, 1));
    assert!(
        earliest <= control && control <= latest,
        "{label}: the undated control carries the server's own time, got {control:?} at {now:?}"
    );
    let stored_of = |back: &Message| -> BTreeSet<Keyword> {
        back.keywords.intersection(&asked).cloned().collect()
    };
    assert_eq!(
        stored_of(&dated_back),
        dated_answer.kept.keywords,
        "{label}: the receipt says exactly what was stored"
    );
    assert_eq!(stored_of(&undated_back), BTreeSet::new(), "{label}");

    for (back, sent) in [(&dated_back, &batch[0]), (&undated_back, &batch[1])] {
        let source = reader
            .fetch_message_source(account, back)
            .await
            .unwrap_or_else(|err| panic!("fetch the source on {label}: {err}"));
        assert_eq!(
            source.as_bytes(),
            sent.raw.as_bytes(),
            "{label}: stored byte for byte"
        );
    }
    Receipts {
        dated: dated_answer,
        undated: undated_answer,
        asked,
    }
}

/// The time now, in whole seconds.
pub(crate) fn utc_now() -> UtcDateTime {
    instant(time::OffsetDateTime::now_utc())
}

/// `at` moved by `days` whole days.
fn shifted(at: UtcDateTime, days: i64) -> UtcDateTime {
    let at = time::PrimitiveDateTime::parse(
        String::from(at).trim_end_matches('Z'),
        &time::format_description::well_known::Iso8601::DEFAULT,
    )
    .expect("an engine instant parses")
    .assume_utc();
    instant(at + time::Duration::days(days))
}

/// `at` as the engine's whole-second UTC instant.
fn instant(at: time::OffsetDateTime) -> UtcDateTime {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
    .parse()
    .expect("a UTC instant in whole seconds")
}

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod tests;
