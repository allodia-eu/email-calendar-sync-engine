//! The mail list read: what a mailbox list is built from, and the only read that answers it.

use engine_core::{
    ids::{MailboxId, ProviderKey, ThreadId},
    mail::MailFlags,
    search_index::{MailRow, MembershipKind, MembershipRow},
    sync::{SyncState, SyncUpdate},
    time::UtcDateTime,
};

use super::super::{TestObject, acct, email_scope, lease_request, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::ManualClock,
    store::{MailSelector, Store, StoreRead},
};

/// A stored row with only the fields a case is about set; the rest are the empty message.
fn row(key: &str, date: Option<&str>, thread: Option<&ThreadId>) -> MailRow {
    MailRow {
        key: pk(key),
        thread_id: thread.cloned(),
        message_id: None,
        date_utc: date.map(|d| d.parse::<UtcDateTime>().unwrap()),
        flags: MailFlags::default(),
        has_attachment: false,
        from_name: None,
        from_addr: None,
        subject: None,
        preview: None,
    }
}

/// Applies `rows` to one scope as mail objects, each filed in `mailbox`.
async fn seed<S: Store + StoreRead>(
    store: &S,
    account: &engine_core::ids::AccountId,
    mailbox: &str,
    cursor: &str,
    rows: Vec<MailRow>,
) {
    let scope = email_scope(account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    for row in &rows {
        derived.memberships.push(MembershipRow {
            key: row.key.clone(),
            kind: MembershipKind::Mailbox,
            value: mailbox.to_owned(),
        });
    }
    let update = SyncUpdate::delta(
        rows.iter()
            .map(|row| TestObject::new(row.key.as_str(), "body"))
            .collect(),
        vec![],
    );
    derived.messages = rows;
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new(cursor)),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();
}

/// The keys of a read, in the order it returned them.
fn keys(rows: &[crate::store::MailListRow]) -> Vec<ProviderKey> {
    rows.iter().map(|row| row.mail.key.clone()).collect()
}

/// `list_mail` orders newest first with undated messages last, caps at `limit`, carries each
/// row's mailbox membership, and drops a message the moment it is tombstoned.
pub(in crate::contract) async fn list_mail_orders_by_date_and_excludes_tombstones<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-maillist");
    seed(
        store,
        &account,
        "inbox",
        "ml-1",
        vec![
            row("m1", Some("2026-01-03T00:00:00Z"), None),
            row("m2", Some("2026-01-01T00:00:00Z"), None),
            row("m3", None, None),
        ],
    )
    .await;

    let accounts = [account.clone()];
    let rows = store
        .list_mail(&accounts, MailSelector::Newest, usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        keys(&rows),
        vec![pk("m1"), pk("m2"), pk("m3")],
        "newest first, and an undated message sorts below every dated one"
    );
    assert_eq!(
        rows[0].mailboxes,
        vec![MailboxId::try_from("inbox").unwrap()],
        "a row carries the collections it is filed in, so a host can tell what is in view"
    );
    assert_eq!(rows[0].account, account);

    // The window is the true newest `limit`, and it is the same window on every read.
    let page = store
        .list_mail(&accounts, MailSelector::Newest, 2)
        .await
        .unwrap();
    assert_eq!(keys(&page), vec![pk("m1"), pk("m2")]);
    assert_eq!(
        keys(
            &store
                .list_mail(&accounts, MailSelector::Newest, 2)
                .await
                .unwrap()
        ),
        keys(&page),
        "an unchanged store returns the same sequence, so a host reconciling by row id sees no \
         movement"
    );

    // Tombstoning clears the row with the object, so it leaves the list.
    let scope = email_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let drop_m1: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![pk("m1")]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(
                &drop_m1,
                &DerivedWrite::empty(),
                &[],
                &SyncState::new("ml-2"),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        keys(
            &store
                .list_mail(&accounts, MailSelector::Newest, usize::MAX)
                .await
                .unwrap()
        ),
        vec![pk("m2"), pk("m3")]
    );
}

/// Several accounts in one call are one ordered answer, not one answer per account: the rows
/// interleave by date, each tagged with the account it came from. An account not named
/// contributes nothing.
pub(in crate::contract) async fn list_mail_merges_accounts_into_one_order<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let a = acct("acct-unified-a");
    let b = acct("acct-unified-b");
    seed(
        store,
        &a,
        "inbox",
        "u-a",
        vec![
            row("a-old", Some("2026-01-01T00:00:00Z"), None),
            row("a-new", Some("2026-01-04T00:00:00Z"), None),
        ],
    )
    .await;
    seed(
        store,
        &b,
        "inbox",
        "u-b",
        vec![
            row("b-mid", Some("2026-01-02T00:00:00Z"), None),
            row("b-newest", Some("2026-01-05T00:00:00Z"), None),
        ],
    )
    .await;

    let rows = store
        .list_mail(&[a.clone(), b.clone()], MailSelector::Newest, usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        keys(&rows),
        vec![pk("b-newest"), pk("a-new"), pk("b-mid"), pk("a-old")],
        "one date order across both accounts"
    );
    assert_eq!(
        rows.iter().map(|r| r.account.clone()).collect::<Vec<_>>(),
        vec![b.clone(), a.clone(), b.clone(), a.clone()]
    );
    assert_eq!(
        keys(
            &store
                .list_mail(std::slice::from_ref(&a), MailSelector::Newest, usize::MAX)
                .await
                .unwrap()
        ),
        vec![pk("a-new"), pk("a-old")],
        "an account not named contributes nothing"
    );
    assert!(
        store
            .list_mail(&[], MailSelector::Newest, usize::MAX)
            .await
            .unwrap()
            .is_empty(),
        "no accounts asked for, no rows returned"
    );
}

/// `MailSelector::Threads` answers "which messages are on these conversations" directly: every
/// member of every named thread, regardless of date, and nothing from a thread that was not
/// asked for, from an unthreaded message, or from an object tombstoned since.
pub(in crate::contract) async fn list_mail_on_threads_gathers_only_the_named_threads<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-threads");
    let alpha = ThreadId::try_from("t-alpha").unwrap();
    let beta = ThreadId::try_from("t-beta").unwrap();
    let absent = ThreadId::try_from("t-absent").unwrap();
    seed(
        store,
        &account,
        "inbox",
        "th-1",
        vec![
            row("m1", Some("2026-01-04T00:00:00Z"), Some(&alpha)),
            row("m2", Some("2026-01-03T00:00:00Z"), Some(&alpha)),
            row("m3", Some("2026-01-02T00:00:00Z"), Some(&beta)),
            row("m4", Some("2026-01-01T00:00:00Z"), None),
        ],
    )
    .await;

    let accounts = [account.clone()];
    assert_eq!(
        keys(
            &store
                .list_mail(
                    &accounts,
                    MailSelector::Threads(std::slice::from_ref(&alpha)),
                    usize::MAX
                )
                .await
                .unwrap()
        ),
        vec![pk("m1"), pk("m2")],
        "both members of one conversation"
    );
    assert_eq!(
        keys(
            &store
                .list_mail(
                    &accounts,
                    MailSelector::Threads(&[alpha.clone(), beta.clone(), absent.clone()]),
                    usize::MAX
                )
                .await
                .unwrap()
        ),
        vec![pk("m1"), pk("m2"), pk("m3")],
        "several conversations merge into one ordered answer; an unknown thread adds nothing"
    );
    assert!(
        store
            .list_mail(&accounts, MailSelector::Threads(&[]), usize::MAX)
            .await
            .unwrap()
            .is_empty(),
        "no threads asked for, no rows returned"
    );
    assert!(
        store
            .list_mail(
                &accounts,
                MailSelector::Threads(std::slice::from_ref(&absent)),
                usize::MAX
            )
            .await
            .unwrap()
            .is_empty(),
        "an unthreaded message is not a member of anything"
    );
}

/// `MailSelector::Keys` resolves named messages whatever their date, so a search hit or an
/// action's target is reachable outside any window. A key the store does not hold is simply
/// absent.
pub(in crate::contract) async fn list_mail_by_keys_resolves_named_messages<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-bykeys");
    seed(
        store,
        &account,
        "inbox",
        "bk-1",
        vec![
            row("m1", Some("2026-01-02T00:00:00Z"), None),
            row("m2", Some("2026-01-01T00:00:00Z"), None),
        ],
    )
    .await;

    let accounts = [account];
    assert_eq!(
        keys(
            &store
                .list_mail(
                    &accounts,
                    MailSelector::Keys(&[pk("m2"), pk("absent")]),
                    usize::MAX
                )
                .await
                .unwrap()
        ),
        vec![pk("m2")],
        "the named message, and nothing for a key the store does not hold"
    );
    assert!(
        store
            .list_mail(&accounts, MailSelector::Keys(&[]), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}
