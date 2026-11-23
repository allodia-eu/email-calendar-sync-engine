//! Gated live integration: importing into a mailbox (`MailImporter`), against every configured
//! IMAP server.
//!
//! The offline suite asserts the `APPEND` sent; only a real server says it takes a dated
//! `APPEND` with keywords into a folder named outside ASCII, stores the bytes and the date as
//! sent, and lists what was stored on a delta from the import cursor. Every claim is read back
//! through the sync seam, never taken from the import's own answer.
//!
//! A server may add keywords of its own (Stalwart files every message `$hasattachment` or
//! `$hasnoattachment`), so keywords are compared against what was asked.
//!
//! The arrival time is pinned in both directions: the dated message must come back with the
//! date sent, and the undated control with the server's own time, so a pass cannot come from a
//! server that happened to stamp the right day.
//!
//! On Stalwart it runs as the scratch account `carol@`, because Alice's folders are
//! count-asserted elsewhere; the Dovecot harness has only Alice, and the folder this creates is
//! deleted at the end. Skips per server when its address variable is unset.

#![cfg(feature = "import")]

#[path = "common/imap_live.rs"]
mod imap_live;

use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, resume_unwind},
    time::{SystemTime, UNIX_EPOCH},
};

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Keyword, Message, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    sync::SyncUpdate,
    time::UtcDateTime,
};
use engine_provider::{
    MailboxEdit, MailboxWrites, Provider,
    import::{MailImporter, MessageImport},
};
use futures_util::FutureExt;
use imap_live::{DOVECOT_REV1, DOVECOT_REV2, LiveProvider, Server, connect_to};

const STALWART_SCRATCH: Server = Server {
    label: "stalwart",
    addr_var: "STALWART_IMAP_ADDR",
    account: "carol@test.local",
    password: "harness-carol-pw",
};

const SERVERS: [Server; 3] = [STALWART_SCRATCH, DOVECOT_REV1, DOVECOT_REV2];

fn account() -> AccountId {
    AccountId::try_from("live-harness").unwrap()
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn keywords(names: &[&str]) -> BTreeSet<Keyword> {
    names
        .iter()
        .map(|name| Keyword::new(*name).unwrap())
        .collect()
}

/// A small message whose bytes are unique to this run, so a read-back can only match it.
fn raw(subject: &str, run: u128) -> RawMime {
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

#[tokio::test]
async fn an_import_stores_the_bytes_flags_keywords_and_date_and_a_delta_from_its_cursor_lists_it() {
    for server in &SERVERS {
        let test = "an_import_stores_…";
        let Some(importer) = connect_to(server, "INBOX", test).await else {
            continue;
        };
        let label = server.label;
        let run = nanos();

        // A folder named outside ASCII and with a space, so rev1 sends it in modified UTF-7.
        let created = importer
            .edit_mailbox(
                &account(),
                &MailboxEdit::Create {
                    name: format!("Imp€rt {run}"),
                    parent: None,
                },
            )
            .await
            .unwrap_or_else(|err| panic!("create the folder on {label}: {err}"));
        let folder: MailboxId = created.mailbox.expect("the create names its folder");

        // The folder goes whether the checks pass or not, so a failed run leaves no folder behind
        // in an account other suites count.
        let checked = AssertUnwindSafe(import_and_read_back(&importer, server, &folder, run))
            .catch_unwind()
            .await;
        importer
            .edit_mailbox(&account(), &MailboxEdit::Delete { target: folder })
            .await
            .unwrap_or_else(|err| panic!("delete the folder on {label}: {err}"));
        if let Err(failed) = checked {
            resume_unwind(failed);
        }
    }
}

/// Imports a dated message and an undated control into `folder`, then reads both back through a
/// provider bound to it, as a delta from the import cursor.
async fn import_and_read_back(
    importer: &LiveProvider,
    server: &Server,
    folder: &MailboxId,
    run: u128,
) {
    let label = server.label;
    let test = "an_import_stores_…";
    let limits = importer.mail_import_limits(&account()).await.unwrap();
    assert_eq!(limits.max_mailboxes().get(), 1, "{label}");
    eprintln!("{label}: stated limits {limits:?}");

    let cursor = importer
        .import_cursor(&account(), folder)
        .await
        .unwrap_or_else(|err| panic!("import cursor on {label}: {err}"));

    let received = "2019-02-03T04:05:06Z".parse::<UtcDateTime>().unwrap();
    let asked = keywords(&["$seen", "$flagged", "$answered", "$label1"]);
    let dated = MessageImport::new(raw("dated", run), Memberships::of_one(folder.clone()))
        .with_metadata(
            MessageMetadata::default()
                .with_keywords(asked.clone())
                .with_received_at(received),
        );
    let undated = MessageImport::new(raw("undated", run), Memberships::of_one(folder.clone()));
    let batch = [dated, undated];

    let answers = importer.import_messages(&account(), &batch).await;

    assert_eq!(answers.len(), 2, "{label}");
    let dated_answer = answers[0]
        .as_ref()
        .unwrap_or_else(|err| panic!("import the dated message on {label}: {err}"));
    let undated_answer = answers[1]
        .as_ref()
        .unwrap_or_else(|err| panic!("import the undated message on {label}: {err}"));
    assert!(!dated_answer.already_there, "{label}");
    eprintln!("{label}: kept {:?}", dated_answer.kept);
    // Every harness server lists every system flag in `PERMANENTFLAGS` and ends it with `\*`
    // (`tests/fixtures/select_inbox_*.txt`), so by the one rule (`permanent.rs`) all is kept.
    assert_eq!(
        dated_answer.kept.keywords, asked,
        "{label}: every keyword asked for is kept"
    );
    assert_eq!(dated_answer.kept.received_at, Some(received), "{label}");
    assert_eq!(undated_answer.kept.received_at, None, "{label}");

    // Read back through a provider bound to the folder, as a delta from the import cursor.
    let reader = connect_to(server, folder.as_str(), test)
        .await
        .expect("configured");
    let scope = reader
        .sync_email(&account(), Some(&cursor))
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
    let find = |answer_key: Option<&engine_core::ids::ProviderKey>| -> Message {
        let key = answer_key.unwrap_or_else(|| panic!("{label}: UIDPLUS names the key"));
        changed
            .iter()
            .find(|message| message.id.key() == key)
            .cloned()
            .unwrap_or_else(|| panic!("{label}: the delta lists {key}"))
    };
    let dated_back = find(dated_answer.key.as_ref());
    let undated_back = find(undated_answer.key.as_ref());

    assert_eq!(
        dated_back.received_at,
        Some(received),
        "{label}: INTERNALDATE as sent"
    );
    let control = undated_back.received_at.expect("the server stamps one");
    assert!(
        control.year() >= 2026,
        "{label}: the undated control carries the server's own time, got {control:?}"
    );
    // Of what was asked, exactly what the receipt says was kept is stored. A server may add
    // its own besides (Stalwart files every message `$hasattachment` or `$hasnoattachment`).
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
            .fetch_message_source(&account(), back)
            .await
            .unwrap_or_else(|err| panic!("fetch the source on {label}: {err}"));
        assert_eq!(
            source.as_bytes(),
            sent.raw.as_bytes(),
            "{label}: stored byte for byte"
        );
    }
}
