//! The keyword-only write: what it moves, and — more to the point — what it leaves alone.

use engine_core::{
    ids::ProviderKey,
    mail::{Keyword, MailFlags, MailKeywordChange, SystemKeyword},
    search_index::{MailRow, MembershipKind, MembershipRow, project_keyword_change},
    sync::{SyncState, SyncUpdate},
    time::UtcDateTime,
};

use super::super::{TestObject, acct, email_scope, lease_request, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::ManualClock,
    store::{MailSelector, Store, StoreRead},
};

/// A fully-populated row, so a write that blanks a column it was not asked to touch shows up.
fn seeded_row(key: &str) -> MailRow {
    MailRow {
        key: pk(key),
        thread_id: None,
        message_id: None,
        date_utc: Some("2026-01-03T00:00:00Z".parse::<UtcDateTime>().unwrap()),
        flags: MailFlags::default(),
        has_attachment: true,
        from_name: Some("Alice".to_owned()),
        from_addr: Some("alice@example.com".to_owned()),
        subject: Some("Quarterly report".to_owned()),
        preview: Some("The numbers you asked for are attached.".to_owned()),
    }
}

fn keyword_membership(key: &ProviderKey, value: &str) -> MembershipRow {
    MembershipRow {
        key: key.clone(),
        kind: MembershipKind::Keyword,
        value: value.to_owned(),
    }
}

/// A keyword-only change rewrites the flags column and the keyword memberships, and **nothing
/// else** — not the row's other columns, not the mailbox membership that decides which folder
/// the message shows in, and not the normalized payload.
///
/// This is the write a mark-read produces. Before it existed the provider re-sent the whole
/// message and the store replaced the whole payload, which is how a flag change could destroy a
/// field the provider had no way to supply.
pub(in crate::contract) async fn keyword_change_moves_flags_and_leaves_the_rest<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-keyword-change");
    let scope = email_scope(&account);
    let key = pk("m1");

    // Seed one message: a full object payload, a full row, a mailbox membership and a user
    // keyword.
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.messages = vec![seeded_row("m1")];
    derived.memberships = vec![
        MembershipRow {
            key: key.clone(),
            kind: MembershipKind::Mailbox,
            value: "inbox".to_owned(),
        },
        keyword_membership(&key, "todo"),
    ];
    let update = SyncUpdate::delta(vec![TestObject::new("m1", "the-original-payload")], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("kc-1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    // Now the change a mark-read produces: `$seen` arrives, `todo` is gone, and no object.
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.push_keyword_change(project_keyword_change(&MailKeywordChange::new(
        key.clone(),
        [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
    )));
    let empty: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &SyncState::new("kc-2")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    let rows = store
        .list_mail(
            core::slice::from_ref(&account),
            MailSelector::Newest,
            usize::MAX,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the message is still listed");
    let listed = &rows[0];
    assert!(
        listed.mail.flags.seen(),
        "the flag the change carried moved"
    );
    assert_eq!(
        listed.mail.subject.as_deref(),
        Some("Quarterly report"),
        "a column the change did not carry must not be blanked"
    );
    assert_eq!(
        listed.mail.preview.as_deref(),
        Some("The numbers you asked for are attached."),
        "the preview is the field a whole-row write destroyed, so it is the one to pin"
    );
    assert_eq!(
        listed.mail.date_utc,
        Some("2026-01-03T00:00:00Z".parse::<UtcDateTime>().unwrap()),
        "the ordering date survives, so the row does not jump to the bottom of the list"
    );
    assert!(listed.mail.has_attachment);
    assert_eq!(
        listed
            .mailboxes
            .iter()
            .map(engine_core::ids::MailboxId::as_str)
            .collect::<Vec<_>>(),
        vec!["inbox"],
        "the mailbox membership is a different kind and is left alone — a mark-read must not \
         drop the message out of its folder"
    );

    // The normalized payload is untouched: this is the write that used to be a whole-object
    // replace, and the reason the phase exists.
    assert_eq!(
        store.object_payload(&scope, &key).await.unwrap(),
        Some(serde_json::json!({ "key": "m1", "data": "the-original-payload" })),
        "a keyword change is not a new object, so the payload is not rewritten"
    );
}

/// A keyword change for a key the store has never seen writes no row.
///
/// It is an `UPDATE`, not an upsert, because the change carries no subject, sender or date: an
/// insert would file a blank row for a message out of the synced window, and that row would
/// then appear in a list with nothing in it.
pub(in crate::contract) async fn keyword_change_for_an_unknown_message_writes_nothing<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-keyword-unknown");
    let scope = email_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.push_keyword_change(project_keyword_change(&MailKeywordChange::new(
        pk("never-synced"),
        [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
    )));
    let empty: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &SyncState::new("ku-1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    let rows = store
        .list_mail(
            core::slice::from_ref(&account),
            MailSelector::Newest,
            usize::MAX,
        )
        .await
        .unwrap();
    assert!(
        rows.is_empty(),
        "a keyword change for an unsynced message must not conjure a blank list row"
    );
    assert!(
        store
            .object_payload(&scope, &pk("never-synced"))
            .await
            .unwrap()
            .is_none()
    );
}
