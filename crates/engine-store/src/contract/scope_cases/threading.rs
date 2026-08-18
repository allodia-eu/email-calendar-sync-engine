//! Threading as mail lands: the component an arrival joins, the re-key a smaller id forces, and
//! the two things that must never be dragged into the graph.
//!
//! Every case here goes through the ordinary apply — there is no separate "derive" call to make.
//! That is the contract: a message is on its conversation the moment the apply that stored it
//! commits, in the same transaction, across every scope of the account.

use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ThreadId},
    mail::{Message, ThreadRef},
    membership::Memberships,
    search_index::project_message,
    sync::{SyncScope, SyncState, SyncUpdate},
};

use super::super::{TestObject, acct, email_scope, lease_request, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::ManualClock,
    store::{MailSelector, Store, StoreRead},
};

/// A message in `mailbox`, owning `owned` and referencing `references` — the IMAP shape, carrying
/// no thread id of its own.
fn message(key: &str, mailbox: &str, owned: &[&str], references: &[&str]) -> Message {
    let mut message = Message::new(
        MessageId::try_from(key).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    message.envelope.message_id = owned
        .iter()
        .map(|id| MessageIdHeader::new(*id).unwrap())
        .collect();
    message.envelope.references = references
        .iter()
        .map(|id| MessageIdHeader::new(*id).unwrap())
        .collect();
    message
}

/// A second mail scope for the same account — the Sent folder to `email_scope`'s Inbox.
fn other_scope(account: &engine_core::ids::AccountId) -> SyncScope {
    SyncScope::ImapMailbox {
        account: account.clone(),
        mailbox: MailboxId::try_from("Sent").unwrap(),
    }
}

/// Applies `messages` to `scope` as one page, through the engine's own projection.
async fn sync_page<S: Store + StoreRead>(
    store: &S,
    account: &engine_core::ids::AccountId,
    scope: &SyncScope,
    cursor: &str,
    messages: &[Message],
) {
    let claim = store
        .claim_sync_scope(account.clone(), scope, lease_request("worker", 300))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    for message in messages {
        derived.push_mail(project_message(message));
    }
    let update = SyncUpdate::delta(
        messages
            .iter()
            .map(|m| TestObject::new(m.id.key().as_str(), "body"))
            .collect(),
        vec![],
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new(cursor)),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();
}

/// The thread id stored for `key`.
async fn thread_of<S: StoreRead>(
    store: &S,
    account: &engine_core::ids::AccountId,
    key: &str,
) -> Option<ThreadId> {
    store
        .list_mail(
            core::slice::from_ref(account),
            MailSelector::Keys(&[pk(key)]),
            usize::MAX,
        )
        .await
        .unwrap()
        .first()
        .and_then(|row| row.mail.thread_id.clone())
}

/// A reply joins its original's conversation even when the two are in **different scopes** and
/// arrived in different syncs — the case the whole design turns on.
///
/// A per-scope pass can never do this: the reply is in Sent, the original in the Inbox, and they
/// are distinct provider objects under distinct leases. The graph is keyed by account for exactly
/// this reason.
pub(in crate::contract) async fn a_reply_joins_its_original_across_scopes<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-thread-cross");
    let inbox = email_scope(&account);
    let sent = other_scope(&account);

    sync_page(
        store,
        &account,
        &inbox,
        "t-1",
        &[message("inbox-1", "inbox", &["a@h"], &[])],
    )
    .await;
    let original = thread_of(store, &account, "inbox-1").await;
    assert_eq!(
        original.as_ref().map(ThreadId::as_str),
        Some("a@h"),
        "the first message names its own thread as it lands"
    );

    // The reply, later, in the other folder.
    sync_page(
        store,
        &account,
        &sent,
        "t-2",
        &[message("sent-1", "sent", &["b@h"], &["a@h"])],
    )
    .await;
    assert_eq!(
        thread_of(store, &account, "sent-1").await,
        original,
        "the reply joined the thread it answers, in another scope"
    );

    // And the thread read gathers both members.
    let members = store
        .list_mail(
            core::slice::from_ref(&account),
            MailSelector::Threads(&[ThreadId::try_from("a@h").unwrap()]),
            usize::MAX,
        )
        .await
        .unwrap();
    assert_eq!(members.len(), 2);
}

/// A late arrival owning a smaller `Message-ID` re-keys the whole component, not just itself.
///
/// The thread id is a function of the component, so every member has to move — a store that
/// re-keyed only the newcomer would split the conversation in two.
pub(in crate::contract) async fn a_smaller_owned_id_rekeys_every_member<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-thread-rekey");
    let inbox = email_scope(&account);
    let sent = other_scope(&account);

    sync_page(
        store,
        &account,
        &inbox,
        "rk-1",
        &[message("m1", "inbox", &["z@h"], &[])],
    )
    .await;
    sync_page(
        store,
        &account,
        &sent,
        "rk-2",
        &[message("m2", "sent", &["m@h"], &["z@h"])],
    )
    .await;
    assert_eq!(
        thread_of(store, &account, "m1").await.unwrap().as_str(),
        "m@h",
        "the smaller of the two owned ids names the thread"
    );

    // A third message owning a smaller id still: everyone moves onto it.
    sync_page(
        store,
        &account,
        &inbox,
        "rk-3",
        &[message("m3", "inbox", &["a@h"], &["z@h"])],
    )
    .await;
    for key in ["m1", "m2", "m3"] {
        assert_eq!(
            thread_of(store, &account, key).await.unwrap().as_str(),
            "a@h",
            "{key} re-keyed onto the smallest owned id in the component"
        );
    }
    assert!(
        store
            .list_mail(
                core::slice::from_ref(&account),
                MailSelector::Threads(&[ThreadId::try_from("m@h").unwrap()]),
                usize::MAX,
            )
            .await
            .unwrap()
            .is_empty(),
        "the superseded thread id resolves to nothing"
    );
}

/// Two messages arriving in the **same page** that share an id land on one thread.
///
/// Neither has a stored thread when the page opens, so this is the case where a store could
/// plausibly leave them apart — and the id they share is one *neither of them owns*, so it also
/// pins that a referenced-only id joins a component while never naming it.
pub(in crate::contract) async fn one_page_that_shares_an_id_is_one_thread<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-thread-page");
    let inbox = email_scope(&account);
    sync_page(
        store,
        &account,
        &inbox,
        "pg-1",
        &[
            message("m1", "inbox", &["b@h"], &["root@h"]),
            message("m2", "inbox", &["c@h"], &["root@h"]),
            message("m3", "inbox", &["z@h"], &[]),
        ],
    )
    .await;

    let first = thread_of(store, &account, "m1").await.unwrap();
    assert_eq!(
        thread_of(store, &account, "m2").await.unwrap(),
        first,
        "two replies to a root nobody has yet still belong together"
    );
    assert_eq!(
        first.as_str(),
        "b@h",
        "and the thread is named after the smallest id a member owns, not the absent root"
    );
    assert_ne!(
        thread_of(store, &account, "m3").await.unwrap(),
        first,
        "an unrelated message is its own conversation"
    );
}

/// A provider-threaded message keeps the provider's id, and nothing derivable can reach it.
///
/// A `References` header pointing at provider-threaded mail must not merge the two: the provider's
/// grouping is authoritative, and a forged header would otherwise join threads it kept apart.
pub(in crate::contract) async fn a_provider_thread_is_never_joined_or_moved<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-thread-provider");
    let inbox = email_scope(&account);

    let mut native = message("jmap-1", "inbox", &["a@h"], &[]);
    native.thread = Some(ThreadRef::provider_assigned(
        ThreadId::try_from("T-provider").unwrap(),
    ));
    sync_page(store, &account, &inbox, "pv-1", &[native]).await;

    // A derivable reply that references it.
    sync_page(
        store,
        &account,
        &inbox,
        "pv-2",
        &[message("imap-1", "inbox", &["b@h"], &["a@h"])],
    )
    .await;

    assert_eq!(
        thread_of(store, &account, "jmap-1").await.unwrap().as_str(),
        "T-provider",
        "the provider's id stands"
    );
    assert_eq!(
        thread_of(store, &account, "imap-1").await.unwrap().as_str(),
        "b@h",
        "the reply threads alone rather than reaching into the provider's conversation"
    );
}

/// A message with no threading headers is its own conversation, named after its key — and a
/// re-send does not move it.
///
/// Nothing can ever share an id with it, so it needs no entry in the graph to stay a singleton.
/// The re-send is the trap: an object that says nothing about threading must not re-key itself.
pub(in crate::contract) async fn a_bare_message_threads_alone_and_stays<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-thread-bare");
    let inbox = email_scope(&account);
    let bare = message("bare-1", "inbox", &[], &[]);

    sync_page(
        store,
        &account,
        &inbox,
        "br-1",
        core::slice::from_ref(&bare),
    )
    .await;
    assert_eq!(
        thread_of(store, &account, "bare-1").await.unwrap().as_str(),
        "bare-1",
        "the provider key is the stable fallback name"
    );

    sync_page(store, &account, &inbox, "br-2", &[bare]).await;
    assert_eq!(
        thread_of(store, &account, "bare-1").await.unwrap().as_str(),
        "bare-1",
        "and a resync leaves it exactly where it was"
    );
}

/// The store can say when a message is in the graph but carries no thread — the one shape an
/// arrival cannot repair, because the component lookup reaches a stored message only through the
/// thread id its row already carries.
///
/// This is what the v10 migration leaves behind for mail the old whole-account pass had not yet
/// grouped, and it is the question `engine-sync` asks once per mail sync so no host has to
/// remember to repair anything. It must be **false** in steady state: an ordinary page threads
/// what it applies, so a store that answers `true` after a normal sync would put every sync into
/// a whole-account rebuild.
pub(in crate::contract) async fn ungrouped_graphed_mail_is_visible_to_the_store<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-thread-ungrouped");
    let inbox = email_scope(&account);

    assert!(
        !store.has_ungrouped_graphed_mail(&account).await.unwrap(),
        "an empty account has nothing ungrouped"
    );

    sync_page(
        store,
        &account,
        &inbox,
        "ug-1",
        &[message("m1", "inbox", &["a@h"], &[])],
    )
    .await;
    assert!(
        !store.has_ungrouped_graphed_mail(&account).await.unwrap(),
        "an ordinary page threads what it applies, so a sync must not leave work behind — a store \
         answering true here would send every later sync through a whole-account rebuild"
    );
}
