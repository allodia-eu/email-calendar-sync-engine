//! Gated live provider-level checks against a real Microsoft Graph account: folder
//! role resolution, message normalization, and the snapshot → delta cursor cycle
//! through the real HTTP client.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set (an OAuth bearer access token, e.g.
//! from `tools/graph-oauth`), so the offline `cargo test --workspace` stays green.
//! There is no CI harness for this (no live Microsoft account in CI); run it
//! locally:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_provider -- --nocapture
//! ```

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, Message, SystemKeyword},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, Provider};
use provider_graph::{GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The throwaway test account's own address — every live send is addressed to it (the
/// account sends only to itself), so nothing leaves the mailbox.
const SELF_ADDRESS: &str = "allodia-e2e@outlook.com";

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A provider bound to the inbox (Graph accepts the well-known alias in the URL).
fn provider(token: String) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn live_mail_folders_resolve_roles() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_folders_resolve_roles: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let mailboxes = provider(token)
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    assert!(mailboxes.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &mailboxes.update else {
        panic!("expected a folder snapshot");
    };
    // Roles resolve by well-known-alias id despite localized display names.
    let roles: BTreeSet<MailboxRole> = objects.iter().filter_map(|m| m.role.clone()).collect();
    assert!(roles.contains(&MailboxRole::Inbox), "inbox role resolved");
    assert!(roles.contains(&MailboxRole::Sent), "sent role resolved");
    // Every folder is top-level (parent nulled against msgfolderroot).
    assert!(objects.iter().all(|m| m.parent.is_none()));
}

#[tokio::test]
async fn live_message_snapshot_then_delta() {
    let Some(token) = token() else {
        eprintln!("skipping live_message_snapshot_then_delta: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // The initial pass is a full snapshot of the inbox.
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    assert!(snapshot.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a message snapshot");
    };
    // The deterministic seed message is present and fully normalized.
    let subjects: BTreeSet<&str> = objects
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    assert!(
        subjects.contains("Fixture: first message"),
        "seed subject missing; got {subjects:?}"
    );
    // Graph mail is single-folder, so every membership has exactly one collection.
    assert!(objects.iter().all(|m| m.mailboxes.len().get() == 1));

    // A delta from the fresh cursor is a delta (not a snapshot).
    let delta = provider
        .sync_email(&account(), Some(&snapshot.next_cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot());
}

#[tokio::test]
async fn live_send_preserves_message_id_end_to_end() {
    let Some(token) = token() else {
        eprintln!("skipping live_send_preserves_message_id_end_to_end: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // A unique, self-addressed draft: the whole point of the MIME send is that our
    // pre-generated Message-ID reaches the wire verbatim, so we prove it does by finding
    // it come back. Uniqueness comes from the wall clock (a test may read it).
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id = MessageIdHeader::new(format!("graph-live-{unique}@allodia-e2e.test")).unwrap();
    let me = EmailAddress::new(SELF_ADDRESS);
    let draft = Draft::new(
        message_id.clone(),
        me.clone(),
        vec![me],
        format!("provider-graph MIME send probe {unique}"),
        "Sent by the provider-graph live test; safe to delete.",
    );

    // `submit_email` returns a receipt with a Message-ID-derived placeholder key (Graph
    // answers 202 with no id) echoing the Message-ID for reconciliation.
    let receipt = provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit_email");
    assert_eq!(receipt.message_id, message_id);
    assert_eq!(
        receipt.email_key.as_str(),
        format!("sent:graph-live-{unique}@allodia-e2e.test")
    );

    // The self-addressed copy lands back in the Inbox; poll for our exact Message-ID to
    // prove Graph preserved it (it is bracket-stripped in the projection, as we generated it).
    let mut found = false;
    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snapshot = provider
            .sync_email(&account(), None)
            .await
            .expect("resync inbox");
        let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
            continue;
        };
        if objects.iter().any(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id.as_str())
        }) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "the sent Message-ID {} never appeared in the inbox — Graph did not preserve it",
        message_id.as_str()
    );
}

/// Sends a unique self-addressed message and polls the inbox until it appears, returning
/// the synced [`Message`] (with its immutable-id provider key). Panics if it never lands.
async fn send_and_await_inbox(provider: &GraphProvider, message_id: &MessageIdHeader) -> Message {
    // One `sendMail` at a time. Two in flight for the same mailbox get throttled, and the loser
    // fails the *send* rather than the thing its test is about — a failure that moves to
    // whichever test lost the race. The lock covers the send only: waiting for delivery is not
    // what Graph throttles, and holding it through the poll would serialize the whole suite
    // behind one mailbox's delivery latency.
    static SENDING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    let me = EmailAddress::new(SELF_ADDRESS);
    let draft = Draft::new(
        message_id.clone(),
        me.clone(),
        vec![me],
        "provider-graph write probe",
        "Sent by the provider-graph live write test; safe to delete.",
    );
    {
        let _one_at_a_time = SENDING.lock().await;
        provider
            .submit_email(&account(), &draft)
            .await
            .expect("submit_email");
    }

    // Outlook's own delivery of a self-addressed message is what is being waited on, and it is
    // occasionally slow — a minute of headroom, because failing here says nothing about the
    // adapter.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snapshot = provider.sync_email(&account(), None).await.expect("resync");
        let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
            continue;
        };
        if let Some(msg) = objects.into_iter().find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id.as_str())
        }) {
            return msg;
        }
    }
    panic!("the sent message never appeared in the inbox");
}

/// Re-syncs the inbox and finds the message with `message_id`, if still present.
async fn find_in_inbox(provider: &GraphProvider, message_id: &MessageIdHeader) -> Option<Message> {
    let snapshot = provider.sync_email(&account(), None).await.expect("resync");
    let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
        return None;
    };
    objects.into_iter().find(|m| {
        m.envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == message_id.as_str())
    })
}

#[tokio::test]
async fn live_mail_edit_marks_moves_and_deletes() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_edit_marks_moves_and_deletes: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token.clone());

    // Land a fresh, unique self-addressed message in the inbox to edit.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id = MessageIdHeader::new(format!("graph-edit-{unique}@allodia-e2e.test")).unwrap();
    let message = send_and_await_inbox(&provider, &message_id).await;
    let key = message.id.key().clone();
    assert!(
        !message.has_system_keyword(SystemKeyword::Seen),
        "a freshly delivered message is unread"
    );

    // Mark it read + flagged; a re-sync must reflect both keyword changes.
    provider
        .edit_mail(
            &account(),
            &MailEdit::SetKeywords {
                target: key.clone(),
                add: [
                    engine_core::mail::Keyword::system(SystemKeyword::Seen),
                    engine_core::mail::Keyword::system(SystemKeyword::Flagged),
                ]
                .into(),
                remove: std::collections::BTreeSet::new(),
            },
        )
        .await
        .expect("set keywords");
    let edited = find_in_inbox(&provider, &message_id)
        .await
        .expect("the message is still in the inbox after a keyword edit");
    assert!(edited.has_system_keyword(SystemKeyword::Seen), "now read");
    assert!(
        edited.has_system_keyword(SystemKeyword::Flagged),
        "now flagged"
    );

    // Move it to Archive; a re-sync of the inbox must no longer find it.
    let archive = archive_folder_id(&provider).await;
    provider
        .edit_mail(&account(), &MailEdit::move_to(key.clone(), archive))
        .await
        .expect("move to archive");
    assert!(
        find_in_inbox(&provider, &message_id).await.is_none(),
        "the moved message left the inbox"
    );

    // Immutable ids survive the move, so the same key deletes it (from Archive) to clean up.
    provider
        .edit_mail(&account(), &MailEdit::delete(key))
        .await
        .expect("permanent delete");
    // (A repeat delete is NOT retried: Graph answers a re-delete of a purged message with
    // `403 ErrorCannotDeleteObject`, not the clean `404` idempotency keys on. That 404
    // path is offline-tested; the outbox's NeedsConfirmation owns the ambiguous retry.)
}

/// Resolves the account's Archive folder id from a folder-list sync.
async fn archive_folder_id(provider: &GraphProvider) -> MailboxId {
    let folders = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    let SyncUpdate::Snapshot { objects, .. } = folders.update else {
        panic!("expected a folder snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.role == Some(MailboxRole::Archive))
        .expect("an archive folder")
        .id
}

#[tokio::test]
async fn live_an_is_read_change_comes_back_as_state_not_a_whole_message() {
    // Graph's delta returns a *full* object for most edits, and an etag-less **partial** for a
    // lightweight property change — notably `isRead`. That partial is a state change: the
    // adapter resolves it through a narrow `$select` rather than re-fetching the whole message,
    // so a mark-read never rewrites the stored payload.
    //
    // Only a live call proves which shape Graph actually sends. The documentation says changed
    // entries are full objects; the etag-less partial is behaviour observed against the real
    // service, and the offline fakes answer canned bytes whatever we send.
    let Some(token) = token() else {
        eprintln!("skipping live_an_is_read_change_...: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id =
        MessageIdHeader::new(format!("graph-state-{unique}@allodia-e2e.test")).unwrap();
    let message = send_and_await_inbox(&provider, &message_id).await;
    let key = message.id.key().clone();

    // Take the cursor *after* the arrival, so the delta below carries the `isRead` change and
    // not the delivery — a newly arrived message is a full entry and is applied whole.
    let cursor = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot")
        .next_cursor;

    provider
        .edit_mail(&account(), &MailEdit::mark_seen(key.clone(), true))
        .await
        .expect("mark read");

    let delta = provider
        .sync_email(&account(), Some(&cursor))
        .await
        .expect("delta after the isRead change");
    let SyncUpdate::Delta {
        changed, patched, ..
    } = &delta.update
    else {
        panic!("expected a delta");
    };

    assert!(
        !changed.iter().any(|m| m.id.key() == &key),
        "an isRead change is not a whole object: it would rewrite the stored payload"
    );
    let state = patched
        .iter()
        .find(|c| c.key == key)
        .expect("the isRead change came back as a state change");
    assert!(
        state
            .state
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(SystemKeyword::Seen)),
        "and it carries the resulting keywords, got {:?}",
        state.state.keywords
    );
    // Graph mail is single-folder and a move mints a new id, so filing is never mutable state
    // here — the change says nothing about it, and the store leaves those rows alone.
    assert!(
        state.state.mailboxes.is_none(),
        "Graph files through identity, so a state change must not claim to move filing"
    );
    // Both revision tokens ride along. `changeKey` is asked for by name; `@odata.etag` is an
    // OData *annotation* and cannot be named in a `$select` at all — so whether the narrow
    // read answers with one is the service's choice, not ours, and only a live call can say.
    // It does, today. This assertion is how we find out if it ever stops, because the etag is
    // the token an `If-Match` quotes and a state change that arrived without one used to blank
    // the stored value outright.
    assert!(
        state.state.revisions.change_key.is_some(),
        "the narrow $select must return the changeKey"
    );
    assert!(
        state.state.revisions.etag.is_some(),
        "the narrow $select returns @odata.etag even though it cannot be selected; if this \
         fails, Graph changed and `message_state.json` needs recapturing"
    );

    provider
        .edit_mail(&account(), &MailEdit::delete(key))
        .await
        .expect("clean up the throwaway");
}

/// A size estimate reaches the caller for the messages that can be large.
///
/// The unit test proves the arithmetic against a captured shape; this proves the two halves
/// that only a real server can: that the delta endpoint accepts the `$expand` at all — it
/// rejects the same request for `PR_MESSAGE_SIZE` with a `400` — and that what it returns is
/// the field the normalizer reads.
#[tokio::test]
async fn live_attachment_bearing_messages_carry_a_size_estimate() {
    let Some(token) = token() else {
        eprintln!("skipping live_attachment_bearing_messages_carry_a_size_estimate: no token");
        return;
    };
    let provider = provider(token);
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot")
        .update
    else {
        panic!("a first sync is a snapshot");
    };

    let with_attachments: Vec<_> = objects.iter().filter(|m| m.has_attachment).collect();
    assert!(
        !with_attachments.is_empty(),
        "the mailbox should hold at least one message with an attachment",
    );
    let sized = with_attachments.iter().filter(|m| m.size.is_some()).count();
    assert_eq!(
        sized,
        with_attachments.len(),
        "every attachment-bearing message should carry an estimate",
    );
    // Some messages carry a size *without* `hasAttachments`, and that is the point rather than
    // a bug: Graph reports that flag false for a message whose only attachments are inline —
    // the embedded images that carry no paperclip and plenty of bytes. The estimate reads the
    // attachments collection, so it sees them; a cap keyed on `hasAttachments` would not.
    let inline_only = objects
        .iter()
        .filter(|m| !m.has_attachment && m.size.is_some())
        .count();
    println!("{inline_only} message(s) sized through inline attachments alone");

    // Nothing is sized blanket-wise: a message with an empty attachments collection has no
    // opinion, which is what leaves it always fetched.
    assert!(
        objects.iter().any(|m| m.size.is_none()),
        "a plain message must not be given a size it did not earn",
    );
    // And no estimate lands below the body allowance it always includes.
    assert!(
        objects
            .iter()
            .filter_map(|m| m.size)
            .all(|size| size >= 128 * 1024),
        "every estimate carries the body allowance",
    );
}
