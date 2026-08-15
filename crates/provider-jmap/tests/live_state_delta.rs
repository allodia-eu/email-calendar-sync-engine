//! Gated live integration: a JMAP delta that reports only a message's **state**.
//!
//! RFC 8621 §4.1 makes `keywords` and `mailboxIds` the only mutable `Email` properties, so an id
//! in `Email/changes`'s `updated` cannot be reporting a content change — its bytes are immutable.
//! The adapter reads those two properties and nothing else, and the store writes the message row
//! and the `membership` junction and leaves the payload alone.
//!
//! Both mutable axes are driven here against a real server, because they are the two shapes that
//! reach the engine as an update of the *same* object: a mark-read, and a move. The move is the
//! one a keyword-only state change would silently lose.
//!
//! Operates on the dedicated `JmapState`/`JmapStateArchive` pair the seed provides for it, so
//! moving a message *out* of a mailbox never disturbs the count-asserted INBOX/Archive/Projects.
//! Per the determinism rule, both are found by their harness-controlled **name**, never by a
//! server-assigned id. Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, ProviderKey},
    mail::{StoredContent, SystemKeyword},
    sync::SyncScope,
};
use engine_provider::{MailEdit, Provider};
use engine_store::{MailListRow, MailSelector, ManualClock, StoreRead, WorkerId};
use engine_sync::sync_mail;
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

type Store = SqliteStore<ManualClock>;

fn worker() -> WorkerId {
    WorkerId::new("jmap-live-state")
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect JMAP")
}

/// The account's stored mail rows — where keywords and filing live.
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

/// The stored payload of one message, decoded.
async fn payload_of(store: &Store, scope: &SyncScope, key: &ProviderKey) -> StoredContent {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("stored content")
}

#[tokio::test]
async fn a_jmap_update_moves_state_and_never_rewrites_the_message() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_jmap_update_moves_state_...: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("jmap-live-state").unwrap();
    let provider = connect(&harness).await;

    // ---- First sync: everything lands as whole objects. ----
    sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("first sync");

    // The dedicated pair this test owns, by name.
    let mailboxes = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("mailboxes")
        .update
        .changed()
        .to_vec();
    let named = |name: &str| {
        mailboxes
            .iter()
            .find(|m| m.name == name)
            .map_or_else(|| panic!("the seeded {name} mailbox"), |m| m.id.clone())
    };
    let home = named("JmapState");
    let archive = named("JmapStateArchive");

    let scope = provider.email_scope(&account);
    let key = by_subject(&rows_in(&store, &account).await, "JMAP state delta subject")
        .mail
        .key
        .clone();

    // Put the message back to a known start. An interrupted run leaves it flagged or in the
    // other mailbox, and a test that asserted its way out of that would fail on the *next*
    // run rather than the one that broke — the same reason the iMIP test clears residue.
    provider
        .edit_mail(&account, &MailEdit::set_flagged(key.clone(), false))
        .await
        .expect("clear any residue flag");
    provider
        .edit_mail(
            &account,
            &MailEdit::MoveTo {
                target: key.clone(),
                destination: home.clone(),
            },
        )
        .await
        .expect("put it back in its home mailbox");
    sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("sync the reset");

    let before = rows_in(&store, &account).await;
    let target = by_subject(&before, "JMAP state delta subject");
    assert!(!target.mail.flags.flagged(), "it starts unflagged");
    assert!(
        target.mailboxes.contains(&home),
        "and in the mailbox this test owns"
    );
    assert!(
        !target.mailboxes.contains(&archive),
        "and not yet in the one it will move to"
    );
    let payload_before = payload_of(&store, &scope, &key).await;

    // ---- Mutate on the server: a flag change. ----
    provider
        .edit_mail(&account, &MailEdit::set_flagged(key.clone(), true))
        .await
        .expect("flag the message");

    let applied = sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("delta after the flag change");
    assert_eq!(
        applied.email.upserted, 0,
        "a flag change rewrites no message: its content cannot have moved"
    );

    let after_flag = rows_in(&store, &account).await;
    let flagged = after_flag
        .iter()
        .find(|r| r.mail.key == key)
        .expect("still present");
    assert!(flagged.mail.flags.flagged(), "the flag landed");
    assert!(
        flagged
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(SystemKeyword::Flagged)),
        "and so did the keyword membership"
    );
    assert_eq!(
        flagged.mail.subject.as_deref(),
        Some("JMAP state delta subject"),
        "the content the delta never sent is untouched"
    );
    assert_eq!(
        payload_of(&store, &scope, &key).await,
        payload_before,
        "and the payload is byte-for-byte what the first sync wrote"
    );

    // ---- Mutate on the server: a move. ----
    // The axis a keyword-only state change would lose. JMAP moves a message under a stable id,
    // so this arrives as an update of the same object, not as a create plus a destroy.
    provider
        .edit_mail(
            &account,
            &MailEdit::MoveTo {
                target: key.clone(),
                destination: archive.clone(),
            },
        )
        .await
        .expect("move the message");

    let applied = sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("delta after the move");
    assert_eq!(
        applied.email.upserted, 0,
        "a move rewrites no message either — only where it is filed changed"
    );

    let after_move = rows_in(&store, &account).await;
    let moved = after_move
        .iter()
        .find(|r| r.mail.key == key)
        .expect("still present after the move");
    assert!(
        moved.mailboxes.contains(&archive),
        "the move landed in the junction, which is where filing lives"
    );
    assert_eq!(
        moved.mailboxes.len(),
        1,
        "and it replaced the old filing rather than adding to it"
    );
    assert!(
        moved.mail.flags.flagged(),
        "the flag set before the move survived it"
    );
    assert_eq!(
        moved.mail.subject.as_deref(),
        Some("JMAP state delta subject"),
        "and the content still has not moved"
    );
    assert_eq!(
        payload_of(&store, &scope, &key).await,
        payload_before,
        "the payload never carried the filing, so a move does not touch it"
    );

    // ---- Put it back, so a re-run starts where this one did. ----
    provider
        .edit_mail(
            &account,
            &MailEdit::MoveTo {
                target: key.clone(),
                destination: home,
            },
        )
        .await
        .expect("move it back");
    provider
        .edit_mail(&account, &MailEdit::set_flagged(key, false))
        .await
        .expect("unflag it");
}
