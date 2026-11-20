//! One mailbox's mail over a span of time, and how far back the store holds it.

use engine_core::{
    ids::MailboxId,
    search_index::{MembershipKind, MembershipRow},
    sync::{SyncState, SyncUpdate},
    time::UtcDateTime,
};

use super::{
    super::{TestObject, acct, email_scope, lease_request, pk},
    mail::{keys, row},
};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::ManualClock,
    read::{MailSelector, StoreRead},
    store::Store,
};

fn at(text: &str) -> UtcDateTime {
    text.parse().unwrap()
}

/// `list_mail` with a mailbox span selects only that mailbox's dated mail within
/// `[since, until)`, newest first and capped; `oldest_in_mailbox` reports the oldest date the
/// mailbox holds, whatever span was asked for.
pub(in crate::contract) async fn a_mailbox_span_reads_one_mailbox_between_two_instants<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-mailbox-span");
    let scope = email_scope(&account);
    // (key, date, mailboxes)
    let mail: [(&str, Option<&str>, &[&str]); 6] = [
        ("s-new", Some("2026-05-01T09:00:00Z"), &["sent"]),
        ("s-mid", Some("2025-06-01T09:00:00Z"), &["sent"]),
        ("s-both", Some("2025-03-01T09:00:00Z"), &["inbox", "sent"]),
        ("s-old", Some("2024-01-01T00:00:00Z"), &["sent"]),
        ("s-undated", None, &["sent"]),
        ("i-mid", Some("2025-06-02T09:00:00Z"), &["inbox"]),
    ];
    let mut derived = DerivedWrite::empty();
    for (key, date, mailboxes) in mail {
        derived.messages.push(row(key, date, None));
        for mailbox in mailboxes {
            derived.memberships.push(MembershipRow {
                key: pk(key),
                kind: MembershipKind::Mailbox,
                value: (*mailbox).to_owned(),
            });
        }
    }
    let update = SyncUpdate::delta(
        mail.iter()
            .map(|(key, ..)| TestObject::new(key, "body"))
            .collect(),
        vec![],
    );
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("span-1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    let accounts = [account.clone()];
    let sent = MailboxId::try_from("sent").unwrap();
    let span = |since: &str, until: &str| MailSelector::Mailbox {
        mailbox: &sent,
        since: at(since),
        until: at(until),
    };

    let rows = store
        .list_mail(
            &accounts,
            span("2024-01-01T00:00:00Z", "2026-05-01T09:00:00Z"),
            usize::MAX,
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&rows),
        vec![pk("s-mid"), pk("s-both"), pk("s-old")],
        "newest first; `since` is inclusive and `until` exclusive; undated mail and another \
         mailbox's mail are not selected"
    );
    let page = store
        .list_mail(
            &accounts,
            span("2020-01-01T00:00:00Z", "2030-01-01T00:00:00Z"),
            2,
        )
        .await
        .unwrap();
    assert_eq!(
        keys(&page),
        vec![pk("s-new"), pk("s-mid")],
        "the newest `limit`"
    );
    assert!(
        store
            .list_mail(
                &accounts,
                span("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"),
                usize::MAX
            )
            .await
            .unwrap()
            .is_empty(),
        "an empty span selects nothing"
    );

    assert_eq!(
        store.oldest_in_mailbox(&account, &sent).await.unwrap(),
        Some(at("2024-01-01T00:00:00Z"))
    );
    assert_eq!(
        store
            .oldest_in_mailbox(&account, &MailboxId::try_from("inbox").unwrap())
            .await
            .unwrap(),
        Some(at("2025-03-01T09:00:00Z")),
        "a message filed in two mailboxes counts for both"
    );
    assert_eq!(
        store
            .oldest_in_mailbox(&account, &MailboxId::try_from("drafts").unwrap())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .oldest_in_mailbox(&acct("acct-never-synced"), &sent)
            .await
            .unwrap(),
        None
    );
}
