//! Gated live integration: the IMAP CONDSTORE/QRESYNC incremental delta against the
//! Stalwart harness.
//!
//! Connects over implicit TLS (QRESYNC is negotiated on connect), snapshot-syncs the
//! dedicated `QResync` seed mailbox, then **mutates it on the server** — re-flags one
//! message and permanently expunges another via `edit_mail` — and runs a second sync.
//! Because the session is QRESYNC, that second sync is an incremental delta
//! (`CHANGEDSINCE`/`VANISHED`): it must reflect *both* the flag change and the expunge
//! in the store **without** a full re-snapshot. Detecting the expunge incrementally is
//! exactly what a non-QRESYNC delta cannot do, so a tombstoned message proves the path.
//!
//! Operates only on `QResync` (seeded by `docker/stalwart/seed.sh` with copies of the
//! first three INBOX fixtures), so it never disturbs the count-asserted
//! INBOX/Archive/Projects. Skips with no `STALWART_IMAP_ADDR`. Per the determinism
//! rule, targets are chosen by harness-controlled **subject**, never by server UID.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::SystemKeyword,
};
use engine_provider::{MailEdit, Provider};
use engine_store::{MailListRow, MailSelector, ManualClock, StoreRead, WorkerId};
use engine_sync::sync_mail;
use provider_imap::{ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio_rustls::{TlsConnector, client::TlsStream};

type Store = SqliteStore<ManualClock>;

/// A TLS connector that accepts the harness's self-signed cert. Test-only; it never
/// touches a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// Connects an `ImapProvider` bound to `mailbox` (QRESYNC negotiated on connect).
async fn connect(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

/// The account's stored mail as **rows**, not payloads.
///
/// A keyword's home is the `message` row and its `keyword` memberships; the stored
/// payload is the provider's word on the message's *content* and deliberately carries
/// none of it. Reading a decoded payload here would assert that a flag never moved and
/// call it a pass.
async fn rows_in(store: &Store, account: &AccountId) -> Vec<MailListRow> {
    store
        .list_mail(
            core::slice::from_ref(account),
            MailSelector::Newest,
            usize::MAX,
        )
        .await
        .unwrap()
}

fn by_subject<'a>(rows: &'a [MailListRow], subject: &str) -> &'a MailListRow {
    rows.iter()
        .find(|r| r.mail.subject.as_deref() == Some(subject))
        .unwrap_or_else(|| panic!("no seeded message with subject {subject:?}"))
}

#[tokio::test]
async fn live_qresync_delta_reconciles_flag_changes_and_expunges() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_qresync_delta_...: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-live-qresync").unwrap();
    let provider = connect(&harness, "QResync").await;
    let worker = || WorkerId::new("imap-live-qresync");

    // ---- Snapshot sync: the three seeded copies land and the cursor records the
    //      HIGHESTMODSEQ baseline (the QRESYNC SELECT carries it). ----
    sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("snapshot sync");

    let before = rows_in(&store, &account).await;
    assert_eq!(before.len(), 3, "QResync was seeded with three messages");

    // Targets by harness-controlled subject (never by server UID).
    let to_flag = by_subject(&before, "Harness baseline message");
    let to_delete = by_subject(&before, "Duplicate Message-ID (copy A)");
    assert!(
        !to_flag.mail.flags.flagged(),
        "the baseline starts unflagged"
    );
    let flagged_key = to_flag.mail.key.clone();
    let deleted_key = to_delete.mail.key.clone();

    // ---- Mutate on the server: flag one message, permanently expunge another. ----
    provider
        .edit_mail(&account, &MailEdit::set_flagged(flagged_key.clone(), true))
        .await
        .expect("flag a message");
    provider
        .edit_mail(&account, &MailEdit::delete(deleted_key.clone()))
        .await
        .expect("expunge a message");

    // ---- QRESYNC delta sync: reconciles BOTH changes incrementally. ----
    sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("qresync delta sync");

    let after = rows_in(&store, &account).await;

    // The expunged copy A is gone — a delta tombstone, which only QRESYNC's VANISHED
    // can deliver without a full re-snapshot.
    assert_eq!(
        after.len(),
        2,
        "the expunged message was tombstoned by the delta"
    );
    assert!(
        !after.iter().any(|r| r.mail.key == deleted_key),
        "the expunged message must not linger in the store"
    );
    // The other duplicate (copy B) is untouched.
    assert!(
        after
            .iter()
            .any(|r| r.mail.subject.as_deref() == Some("Duplicate Message-ID (copy B)")),
        "copy B is unaffected"
    );
    // The baseline now carries \Flagged — the delta applied the flag change.
    let reflagged = after
        .iter()
        .find(|r| r.mail.key == flagged_key)
        .expect("the flagged message is still present");
    assert!(
        reflagged.mail.flags.flagged(),
        "the delta applied the server-side flag change"
    );
    assert!(
        reflagged
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(SystemKeyword::Flagged)),
        "and the keyword membership moved with it"
    );
    // The delta reported state only, so the content it never sent is untouched.
    assert_eq!(
        reflagged.mail.subject.as_deref(),
        Some("Harness baseline message"),
        "a flag change must not rewrite the message"
    );
}

// The self-signed harness cert is trusted via `engine_tls`'s test-only
// accept-any config (see `no_verify_connector`), so no local verifier lives here.
