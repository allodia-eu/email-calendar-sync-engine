//! The state-only write: what it moves, and — more to the point — what it leaves alone.

use engine_core::{
    ids::ProviderKey,
    mail::{Keyword, MailFlags, MailState, MailStateChange, SystemKeyword},
    search_index::{MailRow, MembershipKind, MembershipRow, project_state_change},
    sync::{SyncState, SyncUpdate},
    time::UtcDateTime,
    version::{ModSeq, RevisionTokens},
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
        revisions: RevisionTokens::default(),
        last_modified: None,
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
pub(in crate::contract) async fn state_change_moves_flags_and_leaves_the_rest<
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
    // The provider reports a fresh revision token with the change — an IMAP `MODSEQ` bumps on a
    // flag change — and it must land, or a later conditional write quotes a stale one.
    let state =
        MailState::with_keywords([Keyword::system(SystemKeyword::Seen)].into_iter().collect())
            .revised(
                RevisionTokens {
                    mod_seq: Some(ModSeq::new(77)),
                    ..RevisionTokens::default()
                },
                Some("2026-02-03T04:05:06Z".parse::<UtcDateTime>().unwrap()),
            );
    derived.push_state_change(project_state_change(&MailStateChange::new(
        key.clone(),
        state,
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
        listed.mail.revisions.mod_seq.map(ModSeq::get),
        Some(77),
        "the revision token moved with the state it belongs to"
    );
    assert_eq!(
        listed.mail.last_modified,
        Some("2026-02-03T04:05:06Z".parse::<UtcDateTime>().unwrap())
    );
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

/// A state change for a key the store has never seen writes **nothing at all** — no message
/// row, and no membership rows either.
///
/// The row is an `UPDATE`, not an upsert, because the change carries no subject, sender or date:
/// an insert would file a blank row for a message out of the synced window. The memberships have
/// to follow the same rule for the same reason, and it is the half that is easy to miss — a
/// `DELETE`/`INSERT` on the junction happily writes rows for a message that is not there, and
/// they are invisible to every list read, which joins through the `message` row. On a provider
/// whose sync is windowed and whose deltas are account-global — Gmail's history, JMAP's
/// `Email/changes` — every label change on mail older than the window is one of these, forever.
pub(in crate::contract) async fn state_change_for_an_unknown_message_writes_nothing<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    use engine_core::{ids::MailboxId, membership::Memberships};

    let account = acct("acct-keyword-unknown");
    let scope = email_scope(&account);
    let key = pk("never-synced");
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    // Filing *and* keywords, so both membership kinds are on the table: this is the shape a
    // Gmail `labelsAdded` or a JMAP `Email/changes` update takes.
    let state =
        MailState::with_keywords([Keyword::system(SystemKeyword::Seen)].into_iter().collect())
            .filed_in(Memberships::of_one(MailboxId::try_from("Archive").unwrap()));
    derived.push_state_change(project_state_change(&MailStateChange::new(
        key.clone(),
        state,
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
    assert!(store.object_payload(&scope, &key).await.unwrap().is_none());
    assert!(
        store
            .index_row_counts(&scope, &key)
            .await
            .unwrap()
            .is_empty(),
        "and no derived row of any kind survives it — a membership row for a message the store \
         does not hold is an orphan no read can ever reach or clear"
    );
}

/// A state change replaces the revision tokens it **carries** and leaves the ones it is silent
/// about standing.
///
/// A partial says which tokens moved; `None` in one means *not reported*, never *gone*. Writing
/// it verbatim blanks the token the next conditional write quotes, and a write that quotes
/// nothing is unguarded last-writer-wins — which fails silently, as lost data rather than an
/// error. Every provider produces the silence: Gmail and JMAP carry no token at all on a state
/// change, IMAP's `FLAGS` row carries no `MODSEQ` unless asked, and Graph's narrow `$select` can
/// answer without the `@odata.etag` a full message resource carries.
pub(in crate::contract) async fn state_change_keeps_the_tokens_it_did_not_carry<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    use engine_core::version::{ChangeKey, ETag};

    let account = acct("acct-token-silence");
    let scope = email_scope(&account);
    let key = pk("m1");

    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    let mut seeded = seeded_row("m1");
    seeded.revisions = RevisionTokens {
        etag: Some(ETag::new("W/\"from-the-whole-object\"")),
        change_key: Some(ChangeKey::new("key-1")),
        ..RevisionTokens::default()
    };
    derived.messages = vec![seeded];
    let update = SyncUpdate::delta(vec![TestObject::new("m1", "the-original-payload")], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("ts-1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    // The mark-read: a fresh `changeKey`, no etag, no modification time.
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    let state =
        MailState::with_keywords([Keyword::system(SystemKeyword::Seen)].into_iter().collect())
            .revised(
                RevisionTokens {
                    change_key: Some(ChangeKey::new("key-2")),
                    ..RevisionTokens::default()
                },
                None,
            );
    derived.push_state_change(project_state_change(&MailStateChange::new(
        key.clone(),
        state,
    )));
    let empty: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &SyncState::new("ts-2")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    let listed = store
        .list_mail(
            core::slice::from_ref(&account),
            MailSelector::Keys(core::slice::from_ref(&key)),
            usize::MAX,
        )
        .await
        .unwrap()
        .pop()
        .expect("the message is still there");
    assert_eq!(
        listed
            .mail
            .revisions
            .change_key
            .as_ref()
            .map(ChangeKey::as_str),
        Some("key-2"),
        "the token the change named moved"
    );
    assert_eq!(
        listed.mail.revisions.etag.as_ref().map(ETag::as_str),
        Some("W/\"from-the-whole-object\""),
        "and the token it never mentioned is still there to quote in the next If-Match"
    );
}

/// A state change that **carries filing** moves the message between mailboxes, and still leaves
/// the content alone.
///
/// This is a JMAP or Gmail move: both file a message under a stable id, so an archive reaches
/// the engine as a change to the same object rather than as a new one. The mailbox memberships
/// are replaced, the keyword memberships are replaced independently, and the payload — which no
/// longer carries filing at all — is untouched.
pub(in crate::contract) async fn state_change_carrying_filing_moves_the_message<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    use engine_core::{ids::MailboxId, membership::Memberships};

    let account = acct("acct-filing-change");
    let scope = email_scope(&account);
    let key = pk("m1");

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
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("fc-1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    // The archive: out of the inbox, into Archive, keeping `todo`.
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    let state = MailState::with_keywords([Keyword::new("todo").unwrap()].into_iter().collect())
        .filed_in(Memberships::of_one(MailboxId::try_from("Archive").unwrap()));
    derived.push_state_change(project_state_change(&MailStateChange::new(
        key.clone(),
        state,
    )));
    let empty: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &SyncState::new("fc-2")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    let listed = store
        .list_mail(
            core::slice::from_ref(&account),
            MailSelector::Keys(core::slice::from_ref(&key)),
            usize::MAX,
        )
        .await
        .unwrap()
        .pop()
        .expect("the moved message is still there");

    assert_eq!(
        listed
            .mailboxes
            .iter()
            .map(engine_core::ids::MailboxId::as_str)
            .collect::<Vec<_>>(),
        vec!["Archive"],
        "the move landed: filing is replaced, not added to"
    );
    assert_eq!(
        listed
            .keywords
            .iter()
            .map(Keyword::as_str)
            .collect::<Vec<_>>(),
        vec!["todo"],
        "and the keyword memberships are a different kind, replaced on their own"
    );
    assert_eq!(
        listed.mail.subject.as_deref(),
        Some("Quarterly report"),
        "a move is not a new object, so the content it never sent survives"
    );
    assert_eq!(
        store.object_payload(&scope, &key).await.unwrap(),
        Some(serde_json::json!({ "key": "m1", "data": "the-original-payload" })),
        "and the payload is not rewritten — it never carried the filing to begin with"
    );
}
