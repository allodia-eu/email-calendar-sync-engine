//! A queued keyword change is shown from the moment it is queued, survives what a sync reports
//! while it waits, joins the server's word once accepted, and goes back only when it is refused
//! for good or withdrawn ([`KeywordEdit`]).

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, ProviderKey},
    mail::{Keyword, MailFlags, MailStateChange, SystemKeyword},
    search_index::{MailRow, MembershipKind, MembershipRow, project_state_change},
    sync::{SyncState, SyncUpdate},
    time::UtcDateTime,
    version::RevisionTokens,
    write::{KeywordEdit, PendingOpId, PendingOutcome},
};

use super::super::{TestObject, acct, email_scope, lease_request, pending_op, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::{Clock, ManualClock},
    outbox::PendingOpClaim,
    read::{MailSelector, StoreRead},
    store::Store,
};

const SEEN: SystemKeyword = SystemKeyword::Seen;
const FLAGGED: SystemKeyword = SystemKeyword::Flagged;

fn set(keywords: &[SystemKeyword]) -> BTreeSet<Keyword> {
    keywords.iter().map(|&k| Keyword::system(k)).collect()
}

fn flag(key: &ProviderKey, on: bool) -> KeywordEdit {
    let (add, remove) = if on {
        (set(&[FLAGGED]), BTreeSet::new())
    } else {
        (BTreeSet::new(), set(&[FLAGGED]))
    };
    KeywordEdit::new(key.clone(), add, remove)
}

/// Stores message `key` with `keywords`, as a first sync would.
async fn stored<S: Store>(
    store: &S,
    account: &AccountId,
    key: &ProviderKey,
    keywords: &[SystemKeyword],
) {
    let scope = email_scope(account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("sync", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.messages = vec![MailRow {
        key: key.clone(),
        thread_id: None,
        message_id: None,
        date_utc: Some("2026-01-03T00:00:00Z".parse::<UtcDateTime>().unwrap()),
        flags: MailFlags::from_keywords(&set(keywords)),
        has_attachment: false,
        size_octets: None,
        from_name: None,
        from_addr: None,
        subject: Some("Hello".to_owned()),
        preview: None,
        revisions: RevisionTokens::default(),
        last_modified: None,
    }];
    derived.memberships = set(keywords)
        .into_iter()
        .map(|keyword| MembershipRow {
            key: key.clone(),
            kind: MembershipKind::Keyword,
            value: keyword.as_str().to_owned(),
        })
        .collect();
    let update = SyncUpdate::delta(vec![TestObject::new(key.as_str(), "payload")], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("s1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();
}

/// A sync reporting that `key` now holds exactly `keywords`, read by a pass that began at
/// `observed_from`.
async fn server_reports<S: Store>(
    store: &S,
    account: &AccountId,
    key: &ProviderKey,
    keywords: &[SystemKeyword],
    observed_from: Option<UtcDateTime>,
) {
    let scope = email_scope(account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("sync", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.push_state_change(project_state_change(&MailStateChange::keywords(
        key.clone(),
        set(keywords),
    )));
    let empty: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![]);
    let cursor = SyncState::new("s2");
    let batch = ApplyBatch::new(&empty, &derived, &[], &cursor);
    let batch = match observed_from {
        Some(started) => batch.observed_from(started),
        None => batch,
    };
    store.apply_sync_update(&claim.lease, batch).await.unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();
}

async fn queue<S: Store>(
    store: &S,
    account: &AccountId,
    idempotency: &str,
    edit: KeywordEdit,
) -> PendingOpId {
    let op =
        pending_op(idempotency, &format!("mail:{}", edit.key.as_str())).with_keyword_edit(edit);
    store.enqueue_pending_op(account.clone(), op).await.unwrap()
}

/// Claims op `op` and records `outcome` for it, as a driver would.
async fn settle<S: Store>(
    store: &S,
    account: &AccountId,
    op: PendingOpId,
    outcome: PendingOutcome,
) {
    let PendingOpClaim::Leased(leased) = store
        .claim_pending_op(account.clone(), op, lease_request("driver", 300))
        .await
        .unwrap()
    else {
        panic!("the op must be claimable");
    };
    store.mark_pending_op(&leased.lease, outcome).await.unwrap();
}

fn refused(class: FailureClass) -> PendingOutcome {
    PendingOutcome::Failed {
        class,
        retry_after: None,
    }
}

fn accepted(key: &ProviderKey) -> PendingOutcome {
    PendingOutcome::Succeeded {
        provider_key: key.clone(),
    }
}

/// The system keywords the store shows for `account`'s one message.
async fn shown<S: StoreRead>(store: &S, account: &AccountId) -> (bool, bool) {
    let rows = store
        .list_mail(
            core::slice::from_ref(account),
            MailSelector::Newest,
            usize::MAX,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    (rows[0].mail.flags.seen(), rows[0].mail.flags.flagged())
}

/// The change is shown the moment it is queued, before anything reaches the server, and a sync
/// that reports the server's older state leaves it shown while taking everything else it says.
pub(in crate::contract) async fn a_queued_keyword_change_shows_at_once_and_survives_a_sync<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-kw-queued");
    let key = pk("m1");
    stored(store, &account, &key, &[]).await;

    queue(store, &account, "flag-1", flag(&key, true)).await;
    assert_eq!(
        shown(store, &account).await,
        (false, true),
        "flagged at once"
    );

    // Another client read it meanwhile; the server has not seen the flag yet.
    server_reports(store, &account, &key, &[SEEN], None).await;
    assert_eq!(
        shown(store, &account).await,
        (true, true),
        "the sync's read state lands, and the queued flag stays on top of it"
    );
}

/// A retryable failure parks the op, and the change stays: it is still going to happen. A
/// refusal for good, or a withdrawal, takes it back to what the server last said, including
/// what a sync reported while it waited.
pub(in crate::contract) async fn only_a_change_that_will_not_happen_goes_back<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-kw-refused");
    let key = pk("m1");
    stored(store, &account, &key, &[]).await;
    let op = queue(store, &account, "flag-2", flag(&key, true)).await;

    settle(store, &account, op, refused(FailureClass::RateLimited)).await;
    assert_eq!(
        shown(store, &account).await,
        (false, true),
        "a rate limit parks it, so it stays"
    );

    server_reports(store, &account, &key, &[SEEN], None).await;
    clock.advance(Duration::from_hours(1));
    settle(store, &account, op, refused(FailureClass::Permanent)).await;
    assert_eq!(
        shown(store, &account).await,
        (true, false),
        "refused for good: the flag goes, the read state the server reported stays"
    );

    let other = pk("m2");
    let account = acct("acct-kw-withdrawn");
    stored(store, &account, &other, &[]).await;
    let op = queue(store, &account, "flag-3", flag(&other, true)).await;
    assert!(
        store
            .cancel_pending_op(account.clone(), op)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        shown(store, &account).await,
        (false, false),
        "withdrawn, so never happened"
    );
}

/// An accepted change joins the server's word. A pass that began reading before it was accepted
/// may still report the old state, and must not take it back; a pass that began after is the
/// server's word and wins outright.
pub(in crate::contract) async fn an_accepted_change_outlasts_a_pass_that_read_before_it<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-kw-accepted");
    let key = pk("m1");
    stored(store, &account, &key, &[]).await;
    let op = queue(store, &account, "flag-4", flag(&key, true)).await;

    let before = clock.now();
    clock.advance(Duration::from_secs(5));
    settle(store, &account, op, accepted(&key)).await;
    assert_eq!(shown(store, &account).await, (false, true));

    server_reports(store, &account, &key, &[], Some(before)).await;
    assert_eq!(
        shown(store, &account).await,
        (false, true),
        "a pass that read before the flag landed does not unflag it"
    );

    clock.advance(Duration::from_secs(5));
    let after = clock.now();
    server_reports(store, &account, &key, &[], Some(after)).await;
    assert_eq!(
        shown(store, &account).await,
        (false, false),
        "a pass that read after is the server's word, even when another client unflagged it"
    );
}

/// Changes to one message compose in the order they were queued, and dropping an earlier one
/// leaves the later ones shown.
pub(in crate::contract) async fn queued_changes_compose_and_one_can_drop_out<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-kw-compose");
    let key = pk("m1");
    stored(store, &account, &key, &[FLAGGED]).await;

    let unflag = queue(store, &account, "unflag", flag(&key, false)).await;
    let read = queue(
        store,
        &account,
        "read",
        KeywordEdit::new(key.clone(), set(&[SEEN]), BTreeSet::new()),
    )
    .await;
    assert_eq!(shown(store, &account).await, (true, false));

    settle(store, &account, unflag, refused(FailureClass::Conflict)).await;
    assert_eq!(
        shown(store, &account).await,
        (true, true),
        "the unflag is gone, the read still queued"
    );

    settle(store, &account, read, accepted(&key)).await;
    assert_eq!(shown(store, &account).await, (true, true));
}

/// A change queued for a message the store does not hold yet shows once a sync brings it.
pub(in crate::contract) async fn a_change_to_a_message_not_yet_held_shows_when_it_arrives<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-kw-later");
    let key = pk("m1");
    queue(store, &account, "flag-5", flag(&key, true)).await;

    stored(store, &account, &key, &[SEEN]).await;
    assert_eq!(shown(store, &account).await, (true, true));
}
