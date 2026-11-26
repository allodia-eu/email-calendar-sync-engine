//! Gated live integration: importing into a mailbox (`MailImporter`), against every configured
//! IMAP server.
//!
//! The offline suite asserts the `APPEND` sent; only a real server says it takes a dated
//! `APPEND` with keywords into a folder named outside ASCII, stores the bytes and the date as
//! sent, and lists what was stored on a delta from the import cursor. Those claims are every
//! importer's, so they are the shared conformance check
//! (`engine_provider::import::conformance`), which the JMAP suite runs too; this file adds what
//! is IMAP's own.
//!
//! On Stalwart it runs as the scratch account `carol@`, because Alice's folders are
//! count-asserted elsewhere; the Dovecot harness has only Alice, and the folder this creates is
//! deleted at the end. Skips per server when its address variable is unset.

#![cfg(feature = "import")]

#[path = "common/imap_live.rs"]
mod imap_live;

use std::{
    panic::{AssertUnwindSafe, resume_unwind},
    time::{SystemTime, UNIX_EPOCH},
};

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::{
    MailboxEdit, MailboxWrites,
    import::{MailImporter, conformance},
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
                &MailboxEdit::create(format!("Imp€rt {run}"), None),
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

/// Runs the shared import claims (`engine_provider::import::conformance`) against `folder`,
/// reading back through a provider bound to it, then what is IMAP's own: one mailbox per
/// message, and every keyword kept.
async fn import_and_read_back(
    importer: &LiveProvider,
    server: &Server,
    folder: &MailboxId,
    run: u128,
) {
    let label = server.label;
    let limits = importer.mail_import_limits(&account()).await.unwrap();
    assert_eq!(limits.max_mailboxes().get(), 1, "{label}");
    eprintln!("{label}: stated limits {limits:?}");

    let reader = connect_to(server, folder.as_str(), "an_import_stores_…")
        .await
        .expect("configured");
    let receipts =
        conformance::stores_as_sent(label, importer, &reader, &account(), folder, run).await;
    eprintln!("{label}: kept {:?}", receipts.dated.kept);
    // Every harness server lists every system flag in `PERMANENTFLAGS` and ends it with `\*`
    // (`tests/fixtures/select_inbox_*.txt`), so by the one rule (`permanent.rs`) all is kept.
    assert_eq!(
        receipts.dated.kept.keywords, receipts.asked,
        "{label}: every keyword asked for is kept"
    );
}
